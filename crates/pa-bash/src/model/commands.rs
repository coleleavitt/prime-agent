//! Running one command in the model: taking wrappers off (`env`, `sudo`,
//! `timeout`, `xargs`, `find -exec`, ...), applying what builtins change
//! (`cd`, `export`, `alias`, `hash -p`, function definitions), and
//! descending into the code shells, `eval`, `source`, `ssh`, `watch` and
//! `su -c` run.

use std::ops::Range;

use super::files::{expand, join, read_script};
use super::value::{self, Arg, Lookup, Unknown, Var};
use super::walker::{
    shown_fields, Call, Feed, Field, Share, State, Walker, MAX_CODE_DEPTH, MAX_FUNCTION_WALKS,
    MAX_INVOCATIONS,
};
use super::wrappers::{self, CodeArg, ShellCode};
use super::{Cwd, Layer, Opaque, OpaqueKind, Via};
use crate::syntax::parse::parse;

/// Builtins a `hash -p` entry or an alias never replaces.
const SHADOW_PROOF: [&str; 9] = [
    "alias", "builtin", "command", "eval", "exec", "hash", "source", ".", "type",
];

fn lower_base(text: &str) -> String {
    text.rsplit('/').next().unwrap_or(text).to_lowercase()
}

impl Walker {
    #[expect(
        clippy::too_many_lines,
        reason = "one dispatch over every program the model knows"
    )]
    pub(super) fn run(
        &mut self,
        mut fields: Vec<Field<'_>>,
        mut layers: Vec<Layer>,
        mut call: Call,
        state: &mut State<'_>,
    ) {
        let mut cwd_override: Option<Cwd> = None;
        loop {
            let Some(first) = fields.first() else {
                return;
            };
            let name = match &first.arg {
                Arg::Known(text) => lower_base(text),
                Arg::Pattern(pattern) => {
                    // A command word the shell expands against the filesystem.
                    let dir = state
                        .cwd
                        .dirs
                        .first()
                        .cloned()
                        .unwrap_or_else(|| self.workspace.clone());
                    match expand(&dir, pattern) {
                        Some(matches) if !matches.is_empty() => {
                            // The command word as written: a rule may read
                            // which names its pattern can produce.
                            self.record(
                                &fields,
                                layers.clone(),
                                &call,
                                state,
                                cwd_override.as_ref(),
                                false,
                                Vec::new(),
                            );
                            let mut replaced: Vec<Field<'_>> = matches
                                .iter()
                                .map(|path| Field::of(Arg::Known(path.display().to_string())))
                                .collect();
                            replaced.extend(fields.drain(1..));
                            fields = replaced;
                            continue;
                        }
                        Some(_) | None => {
                            self.record(
                                &fields,
                                layers,
                                &call,
                                state,
                                cwd_override.as_ref(),
                                false,
                                Vec::new(),
                            );
                            return;
                        }
                    }
                }
                Arg::Unknown(unknown) => {
                    let evidence: Vec<String> =
                        fields.iter().map(|field| field.arg.evidence()).collect();
                    let env: Vec<String> =
                        call.env.iter().map(|(_, value)| value.evidence()).collect();
                    // A value spliced into code (`eval "$x"`, `sh -c "$(cat f)"`)
                    // is code assembled at run time.
                    let kind = if unknown.written.contains("__PA_FRAGMENT_") {
                        OpaqueKind::Dynamic
                    } else {
                        OpaqueKind::CommandWord
                    };
                    self.model.opaques.push(Opaque {
                        kind,
                        shown: self.show_code(&shown_fields(&fields)),
                        evidence: format!("{} {}", evidence.join(" "), env.join(" ")),
                        context: self.via.clone(),
                    });
                    self.record(
                        &fields,
                        layers,
                        &call,
                        state,
                        cwd_override.as_ref(),
                        false,
                        Vec::new(),
                    );
                    return;
                }
            };
            let bare = first.arg.known().is_some_and(|text| !text.contains('/'));
            // Aliases, functions and hash entries replace a bare command word.
            if bare && layers.is_empty() && !SHADOW_PROOF.contains(&name.as_str()) {
                let word = first.arg.known().unwrap_or_default().to_string();
                if !self.expanding.contains(&word) {
                    if let Some(body) = state.aliases.get(&word).cloned() {
                        // Bash does not expand an alias inside its own
                        // expansion.
                        let rest = self.joined_code(&fields[1..], state, &call.pieces);
                        let feed = self.current_feed(&call);
                        self.expanding.push(word.clone());
                        let via = Via::Alias { name: word.clone() };
                        let expanded_only = state.expand_aliases;
                        self.code(
                            &format!("{body} {rest}"),
                            via,
                            state,
                            Share::Shared,
                            Some(feed),
                            None,
                        );
                        self.expanding.pop();
                        if expanded_only {
                            return;
                        }
                    }
                }
                if let Some(body) = state.functions.get(&word).cloned() {
                    let walks = self.function_walks.entry(word.clone()).or_insert(0);
                    *walks += 1;
                    if self.depth >= MAX_CODE_DEPTH
                        || *walks > MAX_FUNCTION_WALKS
                        || self.model.invocations.len() >= MAX_INVOCATIONS
                    {
                        // Past the bounds the body is judged on its visible
                        // text, once per function.
                        if self.unwalked.insert(word.clone()) {
                            self.model.opaques.push(Opaque {
                                kind: OpaqueKind::Unparsed,
                                shown: format!("{word} (a function called past the walk bounds)"),
                                evidence: function_text(&word, &state.functions),
                                context: self.via.clone(),
                            });
                        }
                        self.record(
                            &fields,
                            layers,
                            &call,
                            state,
                            cwd_override.as_ref(),
                            false,
                            Vec::new(),
                        );
                        return;
                    }
                    let args: Vec<Arg> =
                        fields[1..].iter().map(|field| field.arg.clone()).collect();
                    let saved = std::mem::replace(&mut state.positional, args);
                    let feed = self.current_feed(&call);
                    self.depth += 1;
                    self.via.push(Via::Function { name: word });
                    self.stdin.push(feed);
                    self.stage_depth += 1;
                    let scope = state.vars.begin_scope(&call.env);
                    self.walk_command(&body, state);
                    state.vars.end_scope(scope);
                    self.stage_depth -= 1;
                    self.stdin.pop();
                    self.via.pop();
                    self.depth -= 1;
                    state.positional = saved;
                    return;
                }
                if let Some(path) = state.hashed.get(&word).cloned() {
                    fields[0] = Field::of(path);
                    continue;
                }
            }
            let args: Vec<Arg> = fields.iter().map(|field| field.arg.clone()).collect();
            if let Some(unwrapped) = wrappers::unwrap(&name, &args) {
                let has_command = unwrapped.consumed < fields.len();
                if unwrapped.runs_nothing {
                    self.record(
                        &fields,
                        layers,
                        &call,
                        state,
                        cwd_override.as_ref(),
                        false,
                        Vec::new(),
                    );
                    return;
                }
                if !has_command {
                    if unwrapped.shell {
                        layers.push(Layer {
                            name: name.clone(),
                            args: args[1..].to_vec(),
                        });
                        let feed = self.current_feed(&call);
                        self.record(
                            &fields[..0],
                            layers,
                            &call,
                            state,
                            cwd_override.as_ref(),
                            true,
                            Vec::new(),
                        );
                        self.shell_stdin(feed, &name, state);
                        return;
                    }
                    if let Some(split) = unwrapped.split.clone() {
                        self.env_split(split, &fields, layers, call, state);
                        return;
                    }
                    self.record(
                        &fields,
                        layers,
                        &call,
                        state,
                        cwd_override.as_ref(),
                        false,
                        Vec::new(),
                    );
                    return;
                }
                layers.push(Layer {
                    name: name.clone(),
                    args: args[1..unwrapped.consumed].to_vec(),
                });
                call.env.extend(unwrapped.assignments);
                if let Some(dir) = unwrapped.chdir {
                    let from = cwd_override.clone().unwrap_or_else(|| state.cwd.clone());
                    cwd_override = Some(relocate(&from, &dir, state));
                }
                if name == "chroot" {
                    cwd_override = Some(Cwd {
                        dirs: Vec::new(),
                        unknown: Some(shown_fields(&fields[..unwrapped.consumed])),
                    });
                }
                fields.drain(..unwrapped.consumed);
                continue;
            }
            match name.as_str() {
                "rtk" => {
                    let skip = if fields.get(1).and_then(|field| field.arg.known()) == Some("proxy")
                    {
                        2
                    } else {
                        1
                    };
                    let skip = skip.min(fields.len());
                    layers.push(Layer {
                        name,
                        args: args[1..skip].to_vec(),
                    });
                    fields.drain(..skip);
                    continue;
                }
                "xargs" | "parallel" => {
                    let (start, replace) = xargs_command(&name, &args);
                    let start = start.min(fields.len());
                    layers.push(Layer {
                        name: name.clone(),
                        args: args[1..start].to_vec(),
                    });
                    let input = Unknown {
                        prefix: String::new(),
                        written: format!("<arguments {name} reads from its stdin>"),
                        source: self.producer_text(),
                        input: true,
                    };
                    fields.drain(..start);
                    if fields.is_empty() {
                        fields.push(Field::of(Arg::Known("echo".to_string())));
                    }
                    if let Feed::Code(text) = self.current_feed(&call) {
                        // The input is text the command fixes (`printf ... |
                        // xargs bash`): run the command with it.
                        self.xargs_known(&text, replace.as_deref(), &fields, &layers, &call, state);
                        return;
                    }
                    match replace {
                        Some(token) => {
                            // Each input line replaces the token: a field that
                            // is exactly the token is data; a token inside a
                            // longer field (`sh -c 'u="{}"; ...'`) is a value
                            // spliced into that text.
                            let mut spliced = None;
                            for field in &mut fields {
                                let Some(text) = field.arg.known().map(str::to_string) else {
                                    continue;
                                };
                                if text == token {
                                    *field = Field::of(Arg::Unknown(Unknown {
                                        written: text,
                                        ..input.clone()
                                    }));
                                } else if text.contains(&token) {
                                    let placeholder = spliced
                                        .get_or_insert_with(|| {
                                            self.placeholder(input.source.clone())
                                        })
                                        .clone();
                                    *field = Field {
                                        code: Some(text.replace(&token, &placeholder)),
                                        ..Field::of(Arg::Unknown(Unknown {
                                            prefix: text
                                                .split(&token)
                                                .next()
                                                .unwrap_or_default()
                                                .to_string(),
                                            written: text.clone(),
                                            ..input.clone()
                                        }))
                                    };
                                }
                            }
                        }
                        None => fields.push(Field::of(Arg::Unknown(input))),
                    }
                    continue;
                }
                "find" | "fd" | "fdfind" => {
                    self.record(
                        &fields,
                        layers.clone(),
                        &call,
                        state,
                        cwd_override.as_ref(),
                        false,
                        Vec::new(),
                    );
                    self.find_exec(&fields, &layers, &call, state, cwd_override.as_ref());
                    return;
                }
                "docker" | "podman" | "kubectl" | "oc" => {
                    if let Some(start) = container_command(&args) {
                        layers.push(Layer {
                            name: name.clone(),
                            args: args[1..start].to_vec(),
                        });
                        cwd_override = Some(Cwd {
                            dirs: Vec::new(),
                            unknown: Some(format!("{name} exec")),
                        });
                        fields.drain(..start);
                        continue;
                    }
                }
                _ => {}
            }
            break;
        }
        let name = fields
            .first()
            .and_then(|field| field.arg.known())
            .map(lower_base)
            .unwrap_or_default();
        let cwd = cwd_override.clone();
        match name.as_str() {
            "cd" | "pushd" | "popd" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                change_directory(&name, &fields, &call.env, state);
            }
            "export" | "declare" | "typeset" | "local" | "readonly" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                let flags = || {
                    fields[1..]
                        .iter()
                        .filter_map(|field| field.arg.known())
                        .filter(|text| text.starts_with('-'))
                };
                let export = match name.as_str() {
                    "export" => !flags().any(|flag| flag.contains('n')),
                    "declare" | "typeset" => flags().any(|flag| flag.contains('x')),
                    _ => false,
                };
                for field in &fields[1..] {
                    declare(field, state, export);
                }
            }
            "unset" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                for field in &fields[1..] {
                    if let Some(variable) = field.arg.known().filter(|text| !text.starts_with('-'))
                    {
                        state.vars.unset(variable);
                        state.functions.remove(variable);
                    }
                }
            }
            "shopt" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
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
            "unalias" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                // bash's `unalias` takes only `-a`; any other option is a
                // usage error that removes nothing.
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
            "read" | "mapfile" | "readarray" | "getopts" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                let source = match &call.feed {
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
            "set" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
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
            "hash" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                if fields[1..].iter().any(|field| {
                    field
                        .arg
                        .known()
                        .is_some_and(|flag| flag.starts_with('-') && flag.contains('r'))
                }) {
                    state.hashed.clear();
                }
                if let Some((path, names)) = hash_registration(&fields) {
                    for registered in names {
                        state.hashed.insert(registered, path.clone());
                    }
                }
            }
            "eval" => {
                let subs = Self::field_subs(&fields[1..], &call);
                let code = self.joined_code(&fields[1..], state, &call.pieces);
                self.record(&fields, layers, &call, state, cwd.as_ref(), false, subs);
                let feed = self.current_feed(&call);
                let scope = state.vars.begin_scope(&call.env);
                self.code(&code, Via::Eval, state, Share::Shared, Some(feed), cwd);
                state.vars.end_scope(scope);
            }
            "source" | "." => {
                let subs = Self::field_subs(&fields[1..fields.len().min(2)], &call);
                let shown = shown_fields(&fields);
                self.record(&fields, layers, &call, state, cwd.as_ref(), false, subs);
                if let Some(path) = fields.get(1).cloned() {
                    let args = fields[2..].iter().map(|field| field.arg.clone()).collect();
                    let feed = self.current_feed(&call);
                    let scope = state.vars.begin_scope(&call.env);
                    self.script_file(
                        &path.arg,
                        shown,
                        state,
                        Share::Shared,
                        cwd,
                        feed,
                        args,
                        Search::PathThenCwd,
                    );
                    state.vars.end_scope(scope);
                }
            }
            "alias" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
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
            "trap" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                let action = fields[1..]
                    .iter()
                    .find(|field| field.arg.known() != Some("--"));
                if let Some(action) = action {
                    if action.arg.known().is_none_or(|text| !text.starts_with('-')) {
                        let code = self.field_code(action, state, &call.pieces);
                        // The handler may run before any later command (a
                        // DEBUG or EXIT trap): what it changes may hold.
                        let mut child = state.clone();
                        self.code(&code, Via::Trap, &mut child, Share::Shared, None, None);
                        state.cwd.union(&child.cwd);
                    }
                }
            }
            "watch" => {
                let start = watch_command(&fields);
                let code = self.joined_code(&fields[start..], state, &call.pieces);
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                let via = Via::Payload {
                    runner: "watch".to_string(),
                };
                self.code(&code, via, state, Share::Copy, None, cwd);
            }
            "su" | "runuser" | "flock" | "script" => {
                let flag = fields[1..].iter().position(|field| {
                    matches!(
                        field.arg.known(),
                        Some("-c" | "--command" | "--session-command")
                    )
                });
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                if let Some(field) = flag.and_then(|at| fields.get(at + 2)) {
                    let code = self.field_code(field, state, &call.pieces);
                    let via = Via::Payload {
                        runner: format!("{name} -c"),
                    };
                    let feed = self.current_feed(&call);
                    self.code(&code, via, state, Share::Copy, Some(feed), cwd);
                }
            }
            "ssh" | "autossh" => {
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                let args: Vec<Arg> = fields.iter().map(|field| field.arg.clone()).collect();
                if let Some((host, start)) = wrappers::ssh_command(&args) {
                    if start < fields.len() {
                        let code = self.joined_code(&fields[start..], state, &call.pieces);
                        let feed = self.current_feed(&call);
                        let host = fields[host].arg.shown();
                        let remote = Cwd {
                            dirs: Vec::new(),
                            unknown: Some(format!("ssh {host}")),
                        };
                        self.code(
                            &code,
                            Via::Remote { host },
                            state,
                            Share::Copy,
                            Some(feed),
                            Some(remote),
                        );
                    }
                }
            }
            shell if wrappers::is_shell(shell) => {
                let args: Vec<Arg> = fields.iter().map(|field| field.arg.clone()).collect();
                // A login or interactive shell runs its startup files first.
                let bash_env = call
                    .env
                    .iter()
                    .rev()
                    .find(|(name, _)| name == "BASH_ENV")
                    .map(|(_, value)| value.clone())
                    .or_else(|| match state.vars.get("BASH_ENV") {
                        Lookup::Value(value) if !value.is_empty() => Some(Arg::Known(value)),
                        Lookup::Value(_) | Lookup::Unset => None,
                        Lookup::Unknown(source) => Some(Arg::Unknown(Unknown {
                            prefix: String::new(),
                            written: "$BASH_ENV".to_string(),
                            source,
                            input: false,
                        })),
                    });
                for startup in startup_files(shell, &args, state.home().as_deref(), bash_env) {
                    let shown = format!(
                        "{} (startup file {})",
                        shown_fields(&fields),
                        startup.shown()
                    );
                    self.script_file(
                        &startup,
                        shown,
                        state,
                        Share::Copy,
                        cwd.clone(),
                        Feed::Kernel,
                        Vec::new(),
                        Search::Literal,
                    );
                }
                match wrappers::shell_code(&args) {
                    ShellCode::Strings(codes) => {
                        let mut subs = Vec::new();
                        let mut texts = Vec::new();
                        for code in &codes {
                            match code {
                                CodeArg::Field(at) => {
                                    subs.extend(Self::field_subs(&fields[*at..=*at], &call));
                                    texts.push(self.field_code(&fields[*at], state, &call.pieces));
                                }
                                CodeArg::Glued(text) => texts.push(text.clone()),
                            }
                        }
                        let first = codes.iter().find_map(|code| match code {
                            CodeArg::Field(at) => Some(*at),
                            CodeArg::Glued(_) => None,
                        });
                        let runner = shown_fields(
                            &fields[..first.unwrap_or(fields.len()).min(fields.len())],
                        );
                        let after = first.map_or(fields.len(), |at| at + 1);
                        let positional: Vec<Arg> = fields[after.min(fields.len())..]
                            .iter()
                            .skip(1)
                            .map(|field| field.arg.clone())
                            .collect();
                        self.record(&fields, layers, &call, state, cwd.as_ref(), false, subs);
                        let feed = self.current_feed(&call);
                        for text in texts {
                            let mut child = state.clone();
                            child.positional.clone_from(&positional);
                            let via = Via::Payload {
                                runner: runner.clone(),
                            };
                            self.code(
                                &text,
                                via,
                                &mut child,
                                Share::Copy,
                                Some(feed.clone()),
                                cwd.clone(),
                            );
                        }
                    }
                    ShellCode::File(at) => {
                        let subs = Self::field_subs(&fields[at..=at], &call);
                        let shown = shown_fields(&fields);
                        self.record(&fields, layers, &call, state, cwd.as_ref(), false, subs);
                        let script = fields[at].arg.clone();
                        let args = fields[at + 1..]
                            .iter()
                            .map(|field| field.arg.clone())
                            .collect();
                        let feed = self.current_feed(&call);
                        self.script_file(
                            &script,
                            shown,
                            state,
                            Share::Copy,
                            cwd,
                            feed,
                            args,
                            Search::CwdThenPath,
                        );
                    }
                    ShellCode::Stdin => {
                        let feed = self.current_feed(&call);
                        let subs = call.redirect_subs.clone();
                        self.record(&fields, layers, &call, state, cwd.as_ref(), true, subs);
                        self.shell_stdin(feed, &shown_fields(&fields), state);
                    }
                }
            }
            _ => {
                self.note_write(&fields, &call, state);
                self.record(
                    &fields,
                    layers,
                    &call,
                    state,
                    cwd.as_ref(),
                    false,
                    Vec::new(),
                );
                // A shell script run by its path is read like `bash path`.
                if let Some(word) = fields[0].arg.known().filter(|word| word.contains('/')) {
                    let base = cwd.clone().unwrap_or_else(|| state.cwd.clone());
                    let script = if word.starts_with('/') {
                        Some(std::path::PathBuf::from(word))
                    } else {
                        base.dirs
                            .iter()
                            .map(|dir| join(dir, word))
                            .find(|full| full.is_file())
                    };
                    if let Some(script) =
                        script.filter(|full| !is_installed(full) && runs_as_shell_script(full))
                    {
                        let shown = shown_fields(&fields);
                        let args = fields[1..].iter().map(|field| field.arg.clone()).collect();
                        let feed = self.current_feed(&call);
                        let path = Arg::Known(script.display().to_string());
                        self.script_file(
                            &path,
                            shown,
                            state,
                            Share::Copy,
                            cwd,
                            feed,
                            args,
                            Search::Literal,
                        );
                    }
                }
            }
        }
    }

    /// `xargs` over input text the command fixes: one run per line with
    /// `-I`, else one run with every word appended.
    fn xargs_known(
        &mut self,
        text: &str,
        replace: Option<&str>,
        fields: &[Field<'_>],
        layers: &[Layer],
        call: &Call,
        state: &mut State<'_>,
    ) {
        const MAX_RUNS: usize = 16;
        if let Some(token) = replace {
            for line in text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(MAX_RUNS)
            {
                let run: Vec<Field<'_>> = fields
                    .iter()
                    .map(|field| match field.arg.known() {
                        Some(known) if known.contains(token) => {
                            Field::of(Arg::Known(known.replace(token, line.trim())))
                        }
                        Some(_) | None => field.clone(),
                    })
                    .collect();
                self.run(run, layers.to_vec(), call.again(), state);
            }
        } else {
            let mut run = fields.to_vec();
            run.extend(
                text.split_whitespace()
                    .take(256)
                    .map(|word| Field::of(Arg::Known(word.to_string()))),
            );
            self.run(run, layers.to_vec(), call.again(), state);
        }
    }

    /// Walk one command (a function body at its call).
    fn walk_command(&mut self, command: &crate::syntax::ast::Command, state: &mut State<'_>) {
        let list = crate::syntax::ast::List {
            items: vec![crate::syntax::ast::AndOr {
                first: crate::syntax::ast::Pipeline {
                    negated: false,
                    commands: vec![command.clone()],
                },
                rest: Vec::new(),
                background: false,
            }],
        };
        self.list(&list, state);
    }

    /// The invocation ranges of the substitutions in `fields`' words.
    fn field_subs(fields: &[Field<'_>], call: &Call) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for field in fields {
            if let Some(word) = field.word {
                if let Some(range) = call.word_subs.get(&(std::ptr::from_ref(word) as usize)) {
                    if !range.is_empty() && !out.contains(range) {
                        out.push(range.clone());
                    }
                }
            }
        }
        out.extend(call.redirect_subs.iter().cloned());
        out
    }

    /// One field as code: its word's text with unknown parts as
    /// placeholders when the word made exactly this field.
    fn field_code(
        &mut self,
        field: &Field<'_>,
        state: &State<'_>,
        pieces: &super::walker::Pieces,
    ) -> String {
        if let Some(code) = &field.code {
            return code.clone();
        }
        match (&field.arg, field.word, field.whole) {
            (Arg::Known(text), _, _) => text.clone(),
            (_, Some(word), true) => self.code_text(word, state, pieces),
            (other, _, _) => self.placeholder(other.evidence()),
        }
    }

    fn joined_code(
        &mut self,
        fields: &[Field<'_>],
        state: &State<'_>,
        pieces: &super::walker::Pieces,
    ) -> String {
        let mut parts = Vec::new();
        for field in fields {
            parts.push(self.field_code(field, state, pieces));
        }
        parts.join(" ")
    }

    /// A shell reading its code from `feed`.
    fn shell_stdin(&mut self, feed: Feed, runner: &str, state: &mut State<'_>) {
        let via = Via::Stdin {
            runner: runner.to_string(),
        };
        match feed {
            Feed::Kernel | Feed::Pipe => {}
            Feed::Code(text) => self.code(&text, via, state, Share::Copy, Some(Feed::Kernel), None),
            Feed::File(path) => {
                let shown = format!("{runner} < {}", path.shown());
                self.script_file(
                    &path,
                    shown,
                    state,
                    Share::Copy,
                    None,
                    Feed::Kernel,
                    Vec::new(),
                    Search::Literal,
                );
            }
            Feed::Piped(evidence) => {
                self.model.opaques.push(Opaque {
                    kind: OpaqueKind::Pipe,
                    shown: runner.to_string(),
                    evidence: format!("{runner} {evidence}"),
                    context: self.via.clone(),
                });
            }
            Feed::Hidden(evidence) => {
                self.model.opaques.push(Opaque {
                    kind: OpaqueKind::Stdin,
                    shown: runner.to_string(),
                    evidence: format!("{runner} {evidence}"),
                    context: self.via.clone(),
                });
            }
        }
    }

    /// `env -S STRING`: the string split into the command's argv (`\_` is
    /// a space; `${NAME}` expands).
    fn env_split(
        &mut self,
        split: Arg,
        fields: &[Field<'_>],
        mut layers: Vec<Layer>,
        call: Call,
        state: &mut State<'_>,
    ) {
        let Arg::Known(text) = split else {
            self.model.opaques.push(Opaque {
                kind: OpaqueKind::Dynamic,
                shown: format!("env -S {}", split.shown()),
                evidence: split.evidence(),
                context: self.via.clone(),
            });
            return;
        };
        let text = literal_tildes(&text.replace("\\_", " "));
        let parsed = parse(&text);
        let words = match parsed
            .list
            .items
            .first()
            .and_then(|item| item.first.commands.first())
        {
            Some(crate::syntax::ast::Command::Simple(inner)) => inner.words.clone(),
            Some(_) | None => Vec::new(),
        };
        if words.is_empty() {
            // `env -S ''`: env with no command prints the environment.
            self.record(fields, layers, &call, state, None, false, Vec::new());
            return;
        }
        layers.push(Layer {
            name: "env".to_string(),
            args: fields[1..].iter().map(|field| field.arg.clone()).collect(),
        });
        let resolver = self.resolver(state, &call.pieces);
        let home = state.home();
        let mut inner = Vec::new();
        for word in &words {
            inner.extend(
                value::fields(word, &resolver, home.as_deref())
                    .into_iter()
                    .map(Field::of),
            );
        }
        self.run(inner, layers, call, state);
    }

    /// `find ... -exec CMD {} ;`, `fd -x CMD`: each executed command.
    fn find_exec(
        &mut self,
        fields: &[Field<'_>],
        layers: &[Layer],
        call: &Call,
        state: &mut State<'_>,
        cwd: Option<&Cwd>,
    ) {
        let fd = fields
            .first()
            .and_then(|field| field.arg.known())
            .is_some_and(|text| lower_base(text).starts_with("fd"));
        let mut index = 1;
        while index < fields.len() {
            let flag = fields[index].arg.known().unwrap_or_default().to_string();
            let exec = if fd {
                matches!(flag.as_str(), "-x" | "--exec" | "-X" | "--exec-batch")
            } else {
                matches!(flag.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
            };
            if !exec {
                index += 1;
                continue;
            }
            let start = index + 1;
            let mut end = start;
            while end < fields.len() && !matches!(fields[end].arg.known(), Some(";" | "+")) {
                end += 1;
            }
            let found = Unknown {
                prefix: String::new(),
                written: "{}".to_string(),
                source: "the paths find matches".to_string(),
                input: true,
            };
            let mut placeholder = false;
            let mut inner: Vec<Field<'_>> = fields[start..end]
                .iter()
                .map(|field| match field.arg.known() {
                    Some(text) if text.contains('{') && text.contains('}') => {
                        placeholder = true;
                        Field::of(Arg::Unknown(Unknown {
                            prefix: text.split('{').next().unwrap_or_default().to_string(),
                            written: text.to_string(),
                            ..found.clone()
                        }))
                    }
                    Some(_) | None => field.clone(),
                })
                .collect();
            if fd && !placeholder {
                inner.push(Field::of(Arg::Unknown(found)));
            }
            let mut layers = layers.to_vec();
            layers.push(Layer {
                name: "find".to_string(),
                args: vec![Arg::Known(flag.clone())],
            });
            let saved = state.cwd.clone();
            if flag.ends_with("dir") {
                state.cwd = Cwd {
                    dirs: Vec::new(),
                    unknown: Some(format!("find {flag}")),
                };
            } else if let Some(cwd) = cwd {
                state.cwd = cwd.clone();
            }
            let inner_call = Call {
                env: call.env.clone(),
                output: call.output.clone(),
                ..Call::bare()
            };
            self.run(inner, layers, inner_call, state);
            state.cwd = saved;
            index = end + 1;
        }
    }

    /// Run the script `path` names: read it when it is in the workspace (or
    /// written earlier by this command, or it is the shell's stdin), else
    /// opaque.
    #[expect(
        clippy::too_many_arguments,
        reason = "a script run carries its whole context"
    )]
    fn script_file(
        &mut self,
        path: &Arg,
        shown: String,
        state: &mut State<'_>,
        share: Share,
        cwd: Option<Cwd>,
        feed: Feed,
        args: Vec<Arg>,
        search: Search,
    ) {
        if matches!(
            path.known(),
            Some("/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0" | "-")
        ) {
            self.shell_stdin(feed, &shown, state);
            return;
        }
        let base = cwd.clone().unwrap_or_else(|| state.cwd.clone());
        let text = match path {
            Arg::Known(text) if text.starts_with('/') => {
                let full = join(std::path::Path::new("/"), text);
                self.written
                    .get(&full)
                    .cloned()
                    .or_else(|| read_script(&full))
            }
            Arg::Known(text) => {
                // bash looks a slash-free name up on PATH: `source` before
                // the current directory, a script argument after it.
                let path_dirs = || match (text.contains('/'), state.vars.get("PATH")) {
                    (false, Lookup::Value(value)) => value
                        .split(':')
                        .map(|entry| {
                            if entry.is_empty() {
                                ".".to_string()
                            } else {
                                entry.to_string()
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let mut candidates: Vec<std::path::PathBuf> = Vec::new();
                let here = |dirs: &[std::path::PathBuf], out: &mut Vec<std::path::PathBuf>| {
                    out.extend(dirs.iter().map(|dir| join(dir, text)));
                };
                let on_path = |out: &mut Vec<std::path::PathBuf>| {
                    for entry in path_dirs() {
                        for dir in &base.dirs {
                            out.push(join(&join(dir, &entry), text));
                        }
                    }
                };
                match search {
                    Search::Literal => here(&base.dirs, &mut candidates),
                    Search::PathThenCwd => {
                        on_path(&mut candidates);
                        here(&base.dirs, &mut candidates);
                    }
                    Search::CwdThenPath => {
                        here(&base.dirs, &mut candidates);
                        on_path(&mut candidates);
                    }
                }
                // The first file that exists is the one bash runs.
                candidates
                    .iter()
                    .find(|full| self.written.contains_key(*full) || full.is_file())
                    .and_then(|full| {
                        self.written
                            .get(full)
                            .cloned()
                            .or_else(|| read_script(full))
                    })
            }
            Arg::Pattern(_) | Arg::Unknown(_) => None,
        };
        let Some(text) = text else {
            self.model.opaques.push(Opaque {
                kind: OpaqueKind::Script,
                shown: shown.clone(),
                evidence: format!("{shown} {}", path.evidence()),
                context: self.via.clone(),
            });
            if share == Share::Shared {
                // A sourced file the guard cannot read may change directory.
                state.cwd.unknown.get_or_insert(shown);
            }
            return;
        };
        let via = Via::Script { path: path.shown() };
        match share {
            Share::Shared => {
                let saved = std::mem::replace(&mut state.positional, args);
                self.code(&text, via, state, Share::Shared, None, cwd);
                state.positional = saved;
            }
            Share::Copy => {
                let mut child = state.clone();
                child.positional = args;
                self.code(&text, via, &mut child, Share::Shared, None, cwd);
            }
        }
    }

    /// Remember text a command writes into a file (`cat > x.sh <<EOF`), so
    /// a later `bash x.sh` in the same command reads it.
    fn note_write(&mut self, fields: &[Field<'_>], call: &Call, state: &State<'_>) {
        let Some(super::Output::File(Arg::Known(path))) = &call.output else {
            return;
        };
        let program = fields
            .first()
            .and_then(|field| field.arg.known())
            .map(lower_base);
        let text = match (program.as_deref(), &call.feed) {
            (Some("cat" | "tee"), Some(Feed::Code(text))) if fields.len() == 1 => {
                Some(text.clone())
            }
            (Some("echo"), _) => Some(
                fields[1..]
                    .iter()
                    .map(|field| field.arg.shown())
                    .collect::<Vec<_>>()
                    .join(" ")
                    + "\n",
            ),
            (Some("printf"), _) if fields.len() == 2 => fields[1].arg.known().map(str::to_string),
            _ => None,
        };
        for dir in &state.cwd.dirs {
            let full = join(dir, path);
            match &text {
                Some(text) => {
                    self.written.insert(full, text.clone());
                }
                None => {
                    self.written.remove(&full);
                }
            }
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
fn relocate(base: &Cwd, target: &Arg, state: &State<'_>) -> Cwd {
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

/// Whether `path` is an installed program (under a system directory): run
/// by its path it is a program like any binary, not the agent's script.
fn is_installed(path: &std::path::Path) -> bool {
    const SYSTEM: [&str; 9] = [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc",
        "/opt",
        "/snap",
        "/nix/store",
    ];
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    SYSTEM.iter().any(|dir| real.starts_with(dir))
}

/// Whether executing `path` runs it as a shell script: an executable file
/// whose `#!` line names a shell (directly or through `env`), or that has
/// no `#!` line (bash then runs it itself).
fn runs_as_shell_script(path: &std::path::Path) -> bool {
    use std::io::Read;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    let mut head = [0u8; 256];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let read = file.read(&mut head).unwrap_or(0);
    let head = &head[..read];
    if head.contains(&0) {
        // A binary.
        return false;
    }
    let Some(line) = head.strip_prefix(b"#!") else {
        return true;
    };
    let line =
        String::from_utf8_lossy(line.split(|byte| *byte == b'\n').next().unwrap_or_default())
            .into_owned();
    let mut words = line.split_whitespace();
    let Some(interpreter) = words.next() else {
        return true;
    };
    let mut name = interpreter.rsplit('/').next().unwrap_or(interpreter);
    if name == "env" {
        name = words
            .find(|word| !word.starts_with('-'))
            .unwrap_or_default();
    }
    wrappers::is_shell(name)
}

/// The visible text of function `name` and of every function it calls,
/// transitively (bounded): the evidence for a call the walk does not follow.
fn function_text(
    name: &str,
    functions: &std::collections::HashMap<String, std::rc::Rc<crate::syntax::ast::Command>>,
) -> String {
    const MAX_TEXT: usize = 256 * 1024;
    let mut text = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue = vec![name.to_string()];
    while let Some(next) = queue.pop() {
        if !seen.insert(next.clone()) || text.len() > MAX_TEXT {
            continue;
        }
        let Some(body) = functions.get(&next) else {
            continue;
        };
        let list = crate::syntax::ast::List {
            items: vec![crate::syntax::ast::AndOr {
                first: crate::syntax::ast::Pipeline {
                    negated: false,
                    commands: vec![(**body).clone()],
                },
                rest: Vec::new(),
                background: false,
            }],
        };
        let body_text = list.visible();
        queue.extend(
            body_text
                .split(|ch: char| ch.is_whitespace() || ";|&()`{}".contains(ch))
                .filter(|word| functions.contains_key(*word))
                .map(str::to_string),
        );
        text.push_str(&body_text);
        text.push('\n');
    }
    text
}

/// Where a script operand is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Search {
    /// Exactly the path given (relative to the current directory).
    Literal,
    /// `source NAME`: PATH, then the current directory.
    PathThenCwd,
    /// `bash NAME`: the current directory, then PATH.
    CwdThenPath,
}

/// The startup files a shell started with `args` reads before its code: a
/// login shell's profile, an interactive bash's rc file (`--rcfile` or
/// `~/.bashrc`), a non-interactive bash's `$BASH_ENV`. Profiles and rc files
/// that do not exist are skipped.
fn startup_files(shell: &str, args: &[Arg], home: Option<&str>, bash_env: Option<Arg>) -> Vec<Arg> {
    let (mut login, mut interactive, mut profile, mut rc) = (false, false, true, true);
    let mut rcfile = None;
    let mut index = 1;
    while index < args.len() {
        let Some(text) = args[index].known() else {
            break;
        };
        match text {
            "--login" => login = true,
            "--interactive" => interactive = true,
            "--noprofile" => profile = false,
            "--norc" => rc = false,
            "--rcfile" | "--init-file" => {
                rcfile = args.get(index + 1).cloned();
                index += 1;
            }
            "-o" | "+o" | "-O" | "+O" => index += 1,
            "--" | "-" => break,
            _ if text.starts_with("--") => {}
            _ if text.starts_with('-') => {
                login |= text.contains('l');
                interactive |= text.contains('i');
                if text.contains('c') {
                    break;
                }
            }
            _ => break,
        }
        index += 1;
    }
    let bash = shell == "bash";
    let mut out = Vec::new();
    let home_file = |name: &str| home.map(|home| std::path::Path::new(home).join(name));
    if login && profile {
        out.push(std::path::PathBuf::from("/etc/profile"));
        let personal: &[&str] = if bash {
            &[".bash_profile", ".bash_login", ".profile"]
        } else {
            &[".profile"]
        };
        if let Some(first) = personal
            .iter()
            .filter_map(|name| home_file(name))
            .find(|path| path.is_file())
        {
            out.push(first);
        }
    } else if interactive && rc && bash {
        if let Some(rcfile) = rcfile {
            return vec![rcfile];
        }
        out.extend(home_file(".bashrc"));
    } else if !interactive && bash {
        // A non-interactive bash sources `$BASH_ENV` first.
        if let Some(file) = bash_env {
            return vec![file];
        }
    }
    out.into_iter()
        .filter(|path| path.is_file())
        .map(|path| Arg::Known(path.display().to_string()))
        .collect()
}

/// Where `watch`'s command starts.
fn watch_command(fields: &[Field<'_>]) -> usize {
    let mut index = 1;
    while index < fields.len() {
        let Some(text) = fields[index].arg.known() else {
            break;
        };
        if text == "--" {
            return index + 1;
        }
        if !text.starts_with('-') {
            break;
        }
        if matches!(
            text,
            "-n" | "--interval" | "-q" | "--equexit" | "-s" | "--shotsdir"
        ) {
            index += 1;
        }
        index += 1;
    }
    index.min(fields.len())
}

/// GNU parallel's options that take a value.
const PARALLEL_VALUE_OPTIONS: [&str; 42] = [
    "--jobs",
    "--max-args",
    "--max-replace-args",
    "--sshlogin",
    "--joblog",
    "--results",
    "--tmpdir",
    "--tempdir",
    "--colsep",
    "--arg-file",
    "--delay",
    "--timeout",
    "--retries",
    "--load",
    "--memfree",
    "--tagstring",
    "--rpl",
    "--delimiter",
    "--profile",
    "--max-procs",
    "--max-chars",
    "--halt",
    "--halt-on-error",
    "--nice",
    "--env",
    "--workdir",
    "--wd",
    "--sshdelay",
    "--sshloginfile",
    "--recstart",
    "--recend",
    "--block",
    "--block-size",
    "--basefile",
    "--arg-sep",
    "--arg-file-sep",
    "--header",
    "--return",
    "--trc",
    "--trim",
    "--compress-program",
    "--decompress-program",
];
/// xargs's options that take a value.
const XARGS_VALUE_OPTIONS: [&str; 9] = [
    "--replace",
    "--max-args",
    "--max-lines",
    "--max-procs",
    "--max-chars",
    "--delimiter",
    "--eof",
    "--arg-file",
    "--process-slot-var",
];

/// Where `xargs`'s (or `parallel`'s) command starts, and its `-I`
/// replacement token.
fn xargs_command(name: &str, args: &[Arg]) -> (usize, Option<String>) {
    let (short, long): (&str, &[&str]) = if name == "parallel" {
        ("jNnLSaICdDEJPs", &PARALLEL_VALUE_OPTIONS)
    } else {
        ("InLPsdEaJ", &XARGS_VALUE_OPTIONS)
    };
    let mut replace = None;
    let mut index = 1;
    while index < args.len() {
        let Some(text) = args[index].known() else {
            break;
        };
        if text == "--" {
            index += 1;
            break;
        }
        if let Some(rest) = text.strip_prefix("--replace=") {
            replace = Some(rest.to_string());
        } else if text.starts_with("--") {
            if long.contains(&text) {
                if text == "--replace" {
                    replace = args.get(index + 1).and_then(Arg::known).map(str::to_string);
                }
                index += 1;
            }
        } else if text.starts_with('-') && text.len() > 1 {
            let letters: Vec<char> = text[1..].chars().collect();
            for (at, letter) in letters.iter().enumerate() {
                if *letter == 'i' && name == "xargs" {
                    // `-i[R]`: an optional glued token.
                    let glued: String = letters[at + 1..].iter().collect();
                    replace = Some(if glued.is_empty() {
                        "{}".to_string()
                    } else {
                        glued
                    });
                    break;
                }
                if short.contains(*letter) {
                    let glued: String = letters[at + 1..].iter().collect();
                    let value = if glued.is_empty() {
                        index += 1;
                        args.get(index).and_then(Arg::known).map(str::to_string)
                    } else {
                        Some(glued)
                    };
                    if *letter == 'I' {
                        replace = value;
                    }
                    break;
                }
            }
        } else {
            break;
        }
        index += 1;
    }
    (index, replace)
}

/// `docker exec [options] CONTAINER CMD...`, `kubectl exec ... -- CMD`:
/// where the command starts.
fn container_command(args: &[Arg]) -> Option<usize> {
    const VALUES: [&str; 8] = [
        "-e",
        "--env",
        "-u",
        "--user",
        "-w",
        "--workdir",
        "--env-file",
        "--detach-keys",
    ];
    let program = lower_base(args.first()?.known()?);
    if args.get(1)?.known()? != "exec" {
        return None;
    }
    if matches!(program.as_str(), "kubectl" | "oc") {
        let dashes = args.iter().position(|arg| arg.known() == Some("--"))?;
        return (dashes + 1 < args.len()).then_some(dashes + 1);
    }
    let mut index = 2;
    while index < args.len() {
        let text = args[index].known()?;
        if VALUES.contains(&text) {
            index += 2;
            continue;
        }
        if text.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    (index + 1 < args.len()).then_some(index + 1)
}

/// `env -S` splits its string without tilde expansion: an unquoted `~`
/// that starts a word is escaped so the shell parse keeps it literal.
fn literal_tildes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut previous: Option<char> = None;
    for ch in text.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '~' && previous.is_none_or(char::is_whitespace) => out.push('\\'),
            Some(_) | None => {}
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}
