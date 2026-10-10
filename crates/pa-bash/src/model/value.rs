//! What a word becomes when the command runs, as far as the text and the
//! kernel environment tell: fixed text, a pathname pattern, or a value only
//! known at run time (with its fixed literal start kept: `offer/$n` always
//! starts with `offer/`).

use std::collections::{BTreeMap, BTreeSet};

use crate::syntax::ast::{Part, Quoting, Word};

/// One argv field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arg {
    /// Fixed text.
    Known(String),
    /// A pathname pattern (unquoted `*`, `?`, `[`): the shell replaces it with
    /// the matching paths, or keeps it as written when nothing matches.
    /// Wildcards that were quoted are escaped with a backslash.
    Pattern(String),
    /// A value decided at run time.
    Unknown(Unknown),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unknown {
    /// The literal text the field starts with whatever the expansion
    /// produces.
    pub prefix: String,
    /// The word as written.
    pub written: String,
    /// The text the unknown value comes from (an assignment, a
    /// substitution's commands): what a check searches for evidence.
    pub source: String,
    /// The value is data the command reads at run time (lines `xargs`
    /// reads, paths `find` matches), not text the script spells.
    pub input: bool,
}

impl Arg {
    pub(crate) fn known(&self) -> Option<&str> {
        match self {
            Arg::Known(text) => Some(text),
            Arg::Pattern(_) | Arg::Unknown(_) => None,
        }
    }

    /// The text to show in a message.
    pub(crate) fn shown(&self) -> String {
        match self {
            Arg::Known(text) => text.clone(),
            Arg::Pattern(pattern) => pattern.clone(),
            Arg::Unknown(unknown) => unknown.written.clone(),
        }
    }

    /// The fixed text the field starts with (all of it when known).
    pub(crate) fn fixed_prefix(&self) -> &str {
        match self {
            Arg::Known(text) => text,
            Arg::Pattern(pattern) => {
                let end = pattern.find(['*', '?', '[', '\\']).unwrap_or(pattern.len());
                &pattern[..end]
            }
            Arg::Unknown(unknown) => &unknown.prefix,
        }
    }

    /// Whether the field can start with `ch` (an unknown field with no
    /// fixed start can start with anything).
    pub(crate) fn may_start_with(&self, ch: char) -> bool {
        match self {
            Arg::Known(text) => text.starts_with(ch),
            Arg::Pattern(_) | Arg::Unknown(_) => {
                let prefix = self.fixed_prefix();
                prefix.is_empty() || prefix.starts_with(ch)
            }
        }
    }

    /// Evidence text: the field as written plus where an unknown value came
    /// from.
    pub(crate) fn evidence(&self) -> String {
        match self {
            Arg::Known(text) | Arg::Pattern(text) => text.clone(),
            Arg::Unknown(unknown) => format!("{} {}", unknown.written, unknown.source),
        }
    }
}

/// A shell variable as the text sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Var {
    /// The value, when it is fixed.
    pub value: Option<String>,
    /// The assignment's text (evidence when the value is not fixed).
    pub source: String,
}

/// Variables the script has set so far, over the kernel environment.
#[derive(Debug, Clone)]
pub(crate) struct Vars<'e> {
    env: &'e BTreeMap<String, String>,
    set: BTreeMap<String, Option<Var>>,
    /// Names the script exported (`export`, `declare -x`, or any
    /// assignment under `set -a`).
    exported: BTreeSet<String>,
    allexport: bool,
}

impl<'e> Vars<'e> {
    pub(crate) fn new(env: &'e BTreeMap<String, String>) -> Self {
        Self {
            env,
            set: BTreeMap::new(),
            exported: BTreeSet::new(),
            allexport: false,
        }
    }

    /// `name`'s value: set by the script, else the kernel's, else unset
    /// (`None` inside means set to an unknown value).
    pub(crate) fn get(&self, name: &str) -> Lookup {
        match self.set.get(name) {
            Some(Some(var)) => match &var.value {
                Some(value) => Lookup::Value(value.clone()),
                None => Lookup::Unknown(var.source.clone()),
            },
            Some(None) => Lookup::Unset,
            // Bash sets IFS itself at startup.
            None if name == "IFS" => Lookup::Value(" \t\n".to_string()),
            None => match self.env.get(name) {
                Some(value) => Lookup::Value(value.clone()),
                None if is_dynamic(name) => Lookup::Unknown(String::new()),
                None => Lookup::Unset,
            },
        }
    }

    pub(crate) fn assign(&mut self, name: &str, var: Var) {
        if self.allexport {
            self.exported.insert(name.to_string());
        }
        self.set.insert(name.to_string(), Some(var));
    }

    /// `export NAME`: later commands see `NAME` in their environment.
    pub(crate) fn export(&mut self, name: &str) {
        self.exported.insert(name.to_string());
    }

    /// Bind a command's prefix assignments (`H=x eval '$H'`) for the code
    /// it runs in this shell; [`Vars::end_scope`] restores what was there.
    pub(crate) fn begin_scope(&mut self, assignments: &[(String, Arg)]) -> Scope {
        let mut saved = Vec::new();
        for (name, value) in assignments {
            let previous = match self.set.get(name) {
                None => Previous::Untouched,
                Some(None) => Previous::Unset,
                Some(Some(var)) => Previous::Set(var.clone()),
            };
            saved.push((name.clone(), previous, self.exported.contains(name)));
            self.set.insert(
                name.clone(),
                Some(Var {
                    value: value.known().map(str::to_string),
                    source: value.shown(),
                }),
            );
            self.exported.insert(name.clone());
        }
        Scope(saved)
    }

    pub(crate) fn end_scope(&mut self, scope: Scope) {
        for (name, previous, exported) in scope.0.into_iter().rev() {
            match previous {
                Previous::Set(var) => self.set.insert(name.clone(), Some(var)),
                Previous::Unset => self.set.insert(name.clone(), None),
                Previous::Untouched => self.set.remove(&name),
            };
            if !exported {
                self.exported.remove(&name);
            }
        }
    }

    /// `set -a` / `set +a`.
    pub(crate) fn set_allexport(&mut self, on: bool) {
        self.allexport = on;
    }

    pub(crate) fn unset(&mut self, name: &str) {
        self.set.insert(name.to_string(), None);
    }

    /// Whether the script itself set (or unset) `name`.
    pub(crate) fn set_by_script(&self, name: &str) -> bool {
        self.set.contains_key(name)
    }

    /// The environment changes the script made (`None`: unset) to the
    /// variables whose names start with `prefix`: an assignment counts only
    /// once the variable is exported, or when the kernel environment
    /// already carried it.
    pub(crate) fn exported_changes(&self, prefix: &str) -> Vec<(String, Option<Arg>)> {
        self.set
            .iter()
            .filter(|(name, var)| {
                name.starts_with(prefix)
                    && (var.is_none()
                        || self.env.contains_key(name.as_str())
                        || self.exported.contains(name.as_str()))
            })
            .map(|(name, var)| {
                let value = var.as_ref().map(|var| match &var.value {
                    Some(value) => Arg::Known(value.clone()),
                    None => Arg::Unknown(Unknown {
                        prefix: String::new(),
                        written: format!("${name}"),
                        source: var.source.clone(),
                        input: false,
                    }),
                });
                (name.clone(), value)
            })
            .collect()
    }

    /// After a branch that may or may not have run: a variable the two
    /// states disagree on is no longer fixed.
    pub(crate) fn merge(&mut self, other: &Vars<'_>) {
        self.exported.extend(other.exported.iter().cloned());
        self.allexport |= other.allexport;
        for (name, var) in &other.set {
            let mine = self.set.get(name);
            if mine == Some(var) {
                continue;
            }
            let mut sources: Vec<String> = Vec::new();
            for candidate in [mine.and_then(Option::as_ref), var.as_ref()]
                .into_iter()
                .flatten()
            {
                sources.push(candidate.source.clone());
            }
            self.set.insert(
                name.clone(),
                Some(Var {
                    value: None,
                    source: sources.join(" "),
                }),
            );
        }
    }
}

/// What [`Vars::begin_scope`] replaced.
#[derive(Debug)]
pub(crate) struct Scope(Vec<(String, Previous, bool)>);

/// What a scoped name held before: nothing the script set, an unset, a value.
#[derive(Debug)]
enum Previous {
    Untouched,
    Unset,
    Set(Var),
}

/// Variables bash sets itself that are not in the kernel environment.
fn is_dynamic(name: &str) -> bool {
    matches!(
        name,
        "PWD"
            | "OLDPWD"
            | "RANDOM"
            | "SECONDS"
            | "LINENO"
            | "BASHPID"
            | "PPID"
            | "REPLY"
            | "UID"
            | "EUID"
            | "HOSTNAME"
            | "BASH_SOURCE"
            | "FUNCNAME"
            | "PIPESTATUS"
            | "SRANDOM"
    ) || matches!(name, "?" | "-" | "$" | "!")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    Value(String),
    /// Set to something the text cannot fix; the assignment's text.
    Unknown(String),
    Unset,
}

/// How a parameter or substitution inside a word evaluates.
pub(crate) trait Resolve {
    fn parameter(&self, name: &str, operator: Option<&Word>) -> Piece;
    /// A `$(...)` / backquoted command's output.
    fn command(&self, part: &Part) -> Piece;
}

/// One expansion's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Text(String),
    /// `"$@"`: one field per element.
    Fields(Vec<String>),
    /// Unknown, with the evidence text it comes from.
    Unknown(String),
}

/// Evaluate `word` into argv fields: brace expansion, tilde, parameter and
/// command expansion, field splitting of unquoted results, patterns.
pub(crate) fn fields(word: &Word, resolve: &dyn Resolve, home: Option<&str>) -> Vec<Arg> {
    let mut out = Vec::new();
    // Brace expansion rewrites only literal parts: an alternative's other
    // parts are the original word's, at the same index, and resolve as such.
    match brace_alternatives(word) {
        Some(alternatives) => {
            for alternative in &alternatives {
                fields_of(alternative, word, resolve, home, &mut out);
            }
        }
        None => fields_of(word, word, resolve, home, &mut out),
    }
    out
}

/// Evaluate `word` as one value with no splitting (an assignment value, a
/// redirect target, a here-string, a double-quoted payload).
pub(crate) fn single(word: &Word, resolve: &dyn Resolve, home: Option<&str>) -> Arg {
    let mut builder = Builder::new(word);
    for (index, part) in word.parts.iter().enumerate() {
        builder.part(part, index, resolve, home, Split::No);
    }
    builder.finish_single()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Split {
    Yes,
    No,
}

struct Builder<'w> {
    word: &'w Word,
    fields: Vec<Arg>,
    text: String,
    pattern: bool,
    unknown: Option<(String, String)>,
    started: bool,
}

impl<'w> Builder<'w> {
    fn new(word: &'w Word) -> Self {
        Self {
            word,
            fields: Vec::new(),
            text: String::new(),
            pattern: false,
            unknown: None,
            started: false,
        }
    }

    fn push_literal(&mut self, text: &str, live_glob: bool) {
        self.started = true;
        if self.unknown.is_some() {
            return;
        }
        for ch in text.chars() {
            if matches!(ch, '*' | '?' | '[' | '\\') {
                if live_glob && ch != '\\' {
                    self.pattern = true;
                    self.text.push(ch);
                } else {
                    self.text.push('\\');
                    self.text.push(ch);
                }
            } else {
                self.text.push(ch);
            }
        }
    }

    fn push_unknown(&mut self, source: &str) {
        self.started = true;
        if let Some((_, sources)) = &mut self.unknown {
            sources.push(' ');
            sources.push_str(source);
        } else {
            let prefix = unescape(&self.text);
            self.unknown = Some((prefix, source.to_string()));
        }
    }

    fn end_field(&mut self) {
        if !self.started {
            return;
        }
        let field = self.take();
        self.fields.push(field);
    }

    fn take(&mut self) -> Arg {
        self.started = false;
        let text = std::mem::take(&mut self.text);
        let pattern = std::mem::take(&mut self.pattern);
        match self.unknown.take() {
            Some((prefix, source)) => Arg::Unknown(Unknown {
                prefix,
                written: self.word.raw.clone(),
                source,
                input: false,
            }),
            None if pattern => Arg::Pattern(text),
            None => Arg::Known(unescape(&text)),
        }
    }

    fn part(
        &mut self,
        part: &Part,
        index: usize,
        resolve: &dyn Resolve,
        home: Option<&str>,
        split: Split,
    ) {
        match part {
            Part::Literal(text, Quoting::Bare) => {
                let mut text = text.as_str();
                let tilde;
                // A tilde prefix runs to the first unquoted `/`; any quoting
                // inside it (`~""/x`, `~'/'x`) leaves it literal.
                let whole_prefix = text.contains('/') || index + 1 == self.word.parts.len();
                if index == 0 && text.starts_with('~') && whole_prefix {
                    let end = text.find('/').unwrap_or(text.len());
                    let user = &text[1..end];
                    if let (true, Some(home)) = (user.is_empty(), home) {
                        tilde = format!("{home}{}", &text[end..]);
                        text = &tilde;
                    } else {
                        self.push_unknown(&text[..end]);
                        text = &text[end..];
                    }
                }
                self.push_literal(text, true);
            }
            Part::Literal(text, Quoting::Single | Quoting::Double) => {
                self.push_literal(text, false);
            }
            Part::Parameter {
                name,
                operator,
                quoted,
            } => {
                let piece = resolve.parameter(name, operator.as_deref());
                self.piece(piece, split == Split::Yes && !quoted);
            }
            Part::Command { quoted, .. } => {
                let piece = resolve.command(part);
                self.piece(piece, split == Split::Yes && !quoted);
            }
            Part::Arithmetic(expression) => {
                self.push_unknown(&expression.visible());
            }
            Part::Process { .. } => {
                let piece = resolve.command(part);
                match piece {
                    Piece::Text(text) => self.push_literal(&text, false),
                    Piece::Fields(texts) => self.push_literal(&texts.join(" "), false),
                    Piece::Unknown(source) => self.push_unknown(&source),
                }
            }
        }
    }

    fn piece(&mut self, piece: Piece, split: bool) {
        match piece {
            Piece::Text(text) if split => {
                let mut pieces = text.split([' ', '\t', '\n']);
                if let Some(first) = pieces.next() {
                    if !first.is_empty() {
                        self.push_literal(first, true);
                    }
                }
                for piece in pieces {
                    self.end_field();
                    if !piece.is_empty() {
                        self.push_literal(piece, true);
                    }
                }
            }
            Piece::Text(text) => {
                self.started = true;
                self.push_literal(&text, false);
            }
            Piece::Fields(texts) => {
                // `"$@"`: each element its own field (the first joins what
                // precedes it, the last what follows); no elements, no field.
                for (index, text) in texts.iter().enumerate() {
                    if index > 0 {
                        self.end_field();
                    }
                    if split {
                        self.piece(Piece::Text(text.clone()), true);
                    } else {
                        self.started = true;
                        self.push_literal(text, false);
                    }
                }
            }
            Piece::Unknown(source) => self.push_unknown(&source),
        }
    }

    fn finish_single(mut self) -> Arg {
        self.started = true;
        self.take()
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn fields_of(
    word: &Word,
    original: &Word,
    resolve: &dyn Resolve,
    home: Option<&str>,
    out: &mut Vec<Arg>,
) {
    let mut builder = Builder::new(word);
    let quoted_empty = word.parts.iter().any(|part| {
        matches!(part, Part::Literal(_, Quoting::Single | Quoting::Double))
            || matches!(part, Part::Parameter { quoted: true, name, .. } if name != "@")
            || matches!(part, Part::Command { quoted: true, .. })
    });
    for (index, part) in word.parts.iter().enumerate() {
        let part = match part {
            Part::Literal(..) => part,
            Part::Parameter { .. }
            | Part::Command { .. }
            | Part::Arithmetic(_)
            | Part::Process { .. } => original.parts.get(index).unwrap_or(part),
        };
        builder.part(part, index, resolve, home, Split::Yes);
    }
    if quoted_empty {
        builder.started = true;
    }
    builder.end_field();
    out.extend(builder.fields);
}

/// Brace expansion bound: more alternatives than this keep the word as
/// written.
const BRACE_CAP: usize = 64;

#[cfg(test)]
thread_local! {
    /// Words [`brace_alternatives`] copied: the work a nested word costs.
    pub(crate) static COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The words brace expansion makes of `word` (the word itself when it has
/// no expandable brace in an unquoted literal part).
/// The words brace expansion makes of `word`, or None when it makes only
/// `word` itself (no unquoted `{`, or more alternatives than the cap). A
/// word is cloned only when it really expands: its parts can hold whole
/// nested substitutions.
fn brace_alternatives(word: &Word) -> Option<Vec<Word>> {
    let braces =
        |part: &Part| matches!(part, Part::Literal(text, Quoting::Bare) if text.contains('{'));
    if !word.parts.iter().any(braces) {
        return None;
    }
    #[cfg(test)]
    COPIES.with(|copies| copies.set(copies.get() + 1));
    let mut words = vec![word.clone()];
    for index in 0..word.parts.len() {
        if !braces(&word.parts[index]) {
            continue;
        }
        let mut next = Vec::new();
        for candidate in words {
            let Some(Part::Literal(text, Quoting::Bare)) = candidate.parts.get(index) else {
                next.push(candidate);
                continue;
            };
            let expanded = expand_braces(text);
            if expanded.len() == 1 && expanded[0] == *text {
                next.push(candidate);
                continue;
            }
            for alternative in expanded {
                let mut copy = candidate.clone();
                copy.parts[index] = Part::Literal(alternative, Quoting::Bare);
                next.push(copy);
            }
            if next.len() > BRACE_CAP {
                return None;
            }
        }
        words = next;
    }
    (words.len() > 1 || words.first() != Some(word)).then_some(words)
}

/// Bash brace expansion of one unquoted text: `a{b,c}d`, `{1..3}`, `{a..c}`.
pub(crate) fn expand_braces(text: &str) -> Vec<String> {
    let mut results = vec![String::new()];
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == '{' {
            if let Some((alternatives, end)) = brace_group(&chars, at) {
                let mut next = Vec::new();
                for prefix in &results {
                    for alternative in &alternatives {
                        for expanded in expand_braces(alternative) {
                            next.push(format!("{prefix}{expanded}"));
                            if next.len() > BRACE_CAP {
                                return vec![text.to_string()];
                            }
                        }
                    }
                }
                results = next;
                at = end + 1;
                continue;
            }
        }
        if chars[at] == '\\' && at + 1 < chars.len() {
            for result in &mut results {
                result.push(chars[at]);
                result.push(chars[at + 1]);
            }
            at += 2;
            continue;
        }
        for result in &mut results {
            result.push(chars[at]);
        }
        at += 1;
    }
    results
}

/// The alternatives of the brace group opened at `open`, and its closing
/// index; `None` when it is not an expansion (`{}`, `{a}`, unbalanced).
fn brace_group(chars: &[char], open: usize) -> Option<(Vec<String>, usize)> {
    let mut depth = 0usize;
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut at = open;
    while at < chars.len() {
        let ch = chars[at];
        match ch {
            '\\' if at + 1 < chars.len() => {
                current.push(ch);
                current.push(chars[at + 1]);
                at += 2;
                continue;
            }
            '{' => {
                depth += 1;
                if depth > 1 {
                    current.push(ch);
                }
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    parts.push(current);
                    if parts.len() >= 2 {
                        return Some((parts, at));
                    }
                    return sequence(&parts[0]).map(|items| (items, at));
                }
                current.push(ch);
            }
            ',' if depth == 1 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
        at += 1;
    }
    None
}

/// `{x..y[..step]}` over integers or single letters.
fn sequence(body: &str) -> Option<Vec<String>> {
    let pieces: Vec<&str> = body.split("..").collect();
    if !(2..=3).contains(&pieces.len()) {
        return None;
    }
    let step: i64 = match pieces.get(2) {
        Some(step) => step.parse::<i64>().ok()?.checked_abs()?,
        None => 1,
    };
    if step == 0 {
        return None;
    }
    let step = usize::try_from(step).ok()?;
    if let (Ok(start), Ok(end)) = (pieces[0].parse::<i64>(), pieces[1].parse::<i64>()) {
        let count = start.abs_diff(end) / u64::try_from(step).ok()? + 1;
        if count > BRACE_CAP as u64 {
            return None;
        }
        let range: Vec<i64> = if start <= end {
            (start..=end).step_by(step).collect()
        } else {
            (end..=start).rev().step_by(step).collect()
        };
        return Some(range.iter().map(ToString::to_string).collect());
    }
    let mut start = pieces[0].chars();
    let mut end = pieces[1].chars();
    let (Some(a), None, Some(b), None) = (start.next(), start.next(), end.next(), end.next())
    else {
        return None;
    };
    if !a.is_ascii_alphabetic() || !b.is_ascii_alphabetic() {
        return None;
    }
    let (low, high) = (u32::from(a.min(b)), u32::from(a.max(b)));
    let mut items: Vec<String> = (low..=high)
        .step_by(step)
        .filter_map(char::from_u32)
        .map(String::from)
        .collect();
    if a > b {
        items.reverse();
    }
    (items.len() <= BRACE_CAP).then_some(items)
}

/// Whether the shell pattern `pattern` (backslash escapes, `*`, `?`,
/// `[...]` with POSIX classes) matches all of `name`.
pub(crate) fn pattern_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    // Iterative wildcard match with one backtrack point for `*`.
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() {
            match pattern[p] {
                '*' => {
                    star = Some((p, n));
                    p += 1;
                    continue;
                }
                '?' => {
                    p += 1;
                    n += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, end)) = class_matches(&pattern, p, name[n]) {
                        if matched {
                            p = end;
                            n += 1;
                            continue;
                        }
                    } else if name[n] == '[' {
                        p += 1;
                        n += 1;
                        continue;
                    }
                }
                '\\' if p + 1 < pattern.len() => {
                    if pattern[p + 1] == name[n] {
                        p += 2;
                        n += 1;
                        continue;
                    }
                }
                ch if ch == name[n] => {
                    p += 1;
                    n += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((star_p, star_n)) => {
                p = star_p + 1;
                n = star_n + 1;
                star = Some((star_p, star_n + 1));
            }
            None => return false,
        }
    }
    pattern[p..].iter().all(|&ch| ch == '*')
}

/// Whether the bracket expression at `open` matches `ch`, and where it
/// ends; `None` when the bracket is not closed (a literal `[`).
fn class_matches(pattern: &[char], open: usize, ch: char) -> Option<(bool, usize)> {
    let mut at = open + 1;
    let negated = matches!(pattern.get(at), Some('!' | '^'));
    if negated {
        at += 1;
    }
    let mut matched = false;
    let mut first = true;
    while at < pattern.len() {
        let current = pattern[at];
        if current == ']' && !first {
            return Some((matched != negated, at + 1));
        }
        first = false;
        if current == '[' && pattern.get(at + 1) == Some(&':') {
            let rest: String = pattern[at + 2..].iter().collect();
            if let Some(end) = rest.find(":]") {
                let class = &rest[..end];
                matched |= match class {
                    "alpha" => ch.is_alphabetic(),
                    "digit" => ch.is_ascii_digit(),
                    "alnum" => ch.is_alphanumeric(),
                    "upper" => ch.is_uppercase(),
                    "lower" => ch.is_lowercase(),
                    "space" => ch.is_whitespace(),
                    "punct" => ch.is_ascii_punctuation(),
                    "xdigit" => ch.is_ascii_hexdigit(),
                    "graph" => !ch.is_whitespace() && !ch.is_control(),
                    "print" => !ch.is_control(),
                    "cntrl" => ch.is_control(),
                    "blank" => ch == ' ' || ch == '\t',
                    _ => false,
                };
                at += 2 + end + 2;
                continue;
            }
        }
        let low = if current == '\\' && at + 1 < pattern.len() {
            at += 1;
            pattern[at]
        } else {
            current
        };
        if pattern.get(at + 1) == Some(&'-') && pattern.get(at + 2).is_some_and(|&high| high != ']')
        {
            let high = pattern[at + 2];
            matched |= low <= ch && ch <= high;
            at += 3;
        } else {
            matched |= low == ch;
            at += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parse::parse;

    struct Env<'a>(&'a Vars<'a>);

    impl Resolve for Env<'_> {
        fn parameter(&self, name: &str, _operator: Option<&Word>) -> Piece {
            match self.0.get(name) {
                Lookup::Value(value) => Piece::Text(value),
                Lookup::Unknown(source) => Piece::Unknown(source),
                Lookup::Unset => Piece::Text(String::new()),
            }
        }

        fn command(&self, _part: &Part) -> Piece {
            Piece::Unknown("cmd".to_string())
        }
    }

    fn argv(text: &str, vars: &Vars<'_>) -> Vec<Arg> {
        let parsed = parse(text);
        let crate::syntax::ast::Command::Simple(command) = &parsed.list.items[0].first.commands[0]
        else {
            panic!("simple");
        };
        command
            .words
            .iter()
            .flat_map(|word| fields(word, &Env(vars), Some("/home/u")))
            .collect()
    }

    fn known(list: &[&str]) -> Vec<Arg> {
        list.iter()
            .map(|text| Arg::Known((*text).to_string()))
            .collect()
    }

    #[test]
    fn words_with_a_literal_start_keep_it() {
        let env = BTreeMap::new();
        let mut vars = Vars::new(&env);
        vars.assign(
            "n",
            Var {
                value: None,
                source: "a b".to_string(),
            },
        );
        let args = argv("git push -u origin offer/$n", &vars);
        assert!(!args[4].may_start_with('-') && !args[4].may_start_with('+'));
        assert_eq!(args[4].fixed_prefix(), "offer/");
        let args = argv("git push $n", &vars);
        assert!(args[2].may_start_with('-') && args[2].may_start_with('+'));
    }

    #[test]
    fn splitting_braces_tilde_and_quotes() {
        let env = BTreeMap::from([("F".to_string(), "-f origin".to_string())]);
        let vars = Vars::new(&env);
        assert_eq!(
            argv("git push $F", &vars),
            known(&["git", "push", "-f", "origin"])
        );
        assert_eq!(
            argv("git push \"$F\"", &vars),
            known(&["git", "push", "-f origin"])
        );
        assert_eq!(argv("su{d,}o id", &vars), known(&["sudo", "suo", "id"]));
        assert_eq!(
            argv("echo ~/x '~' \"\" $EMPTY", &vars),
            known(&["echo", "/home/u/x", "~", ""])
        );
        assert_eq!(
            argv("ls *.rs '*'", &vars)[1..],
            [Arg::Pattern("*.rs".into()), Arg::Known("*".into())]
        );
    }

    #[test]
    fn only_words_with_unquoted_braces_are_copied() {
        let word = |text: &str| {
            let parsed = parse(text);
            let crate::syntax::ast::Command::Simple(command) =
                &parsed.list.items[0].first.commands[0]
            else {
                panic!("simple");
            };
            command.words[1].clone()
        };
        assert!(brace_alternatives(&word("echo \"$(a) {x,y}\"")).is_none());
        assert!(brace_alternatives(&word("echo $(a)`b`")).is_none());
        assert!(brace_alternatives(&word("echo {a}")).is_none());
        assert_eq!(
            brace_alternatives(&word("echo {a,b}$(x)")).map(|words| words.len()),
            Some(2)
        );
    }

    #[test]
    fn patterns_match_like_the_shell() {
        assert!(pattern_matches("[s]udo", "sudo"));
        assert!(pattern_matches("sud?", "sudo"));
        assert!(pattern_matches("[[:lower:]]udo", "sudo"));
        assert!(pattern_matches("*", "sudo"));
        assert!(!pattern_matches("su?do", "sudo"));
        assert!(!pattern_matches("\\*", "sudo"));
        assert!(pattern_matches("[!a]udo", "sudo"));
        assert_eq!(expand_braces("s{u..x}do").len(), 4);
        assert_eq!(expand_braces("{1..3}"), vec!["1", "2", "3"]);
        assert_eq!(expand_braces("{a}"), vec!["{a}"]);
    }
}
