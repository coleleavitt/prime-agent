//! Secret-echo rule: a command whose output reaches the transcript is
//! refused when it prints the whole environment (`env`, `printenv`, `export
//! -p`, `set`, `declare -p`) or a secret file's content (a private SSH key,
//! `~/.aws/credentials`, a `GnuPG` private key, a token file, a process's
//! `environ`). Output that goes to a file, into a variable, or through a
//! filter (`env | grep KEY`, `cut -d= -f1`, `wc -l`) does not reach the
//! transcript whole and is allowed.

use std::path::{Component, Path};

use super::{opaque, Check, Rule};
use crate::model::evidence::find_command_word;
use crate::model::files::join;
use crate::model::value::pattern_matches;
use crate::model::{Arg, Invocation, Model, OpaqueKind, Output};
use crate::verdict::GuardKind;

pub(crate) struct SecretEcho;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SECRET_ECHO appeared after kernel start and is ignored; the secret-echo guard only honors it when the kernel is started with it set.";

impl Rule for SecretEcho {
    const GUARD: GuardKind = GuardKind::SecretEcho;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let home = check
            .context
            .var("HOME")
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from);
        for invocation in &check.model.invocations {
            let place = invocation.place();
            let place = if place.is_empty() {
                String::new()
            } else {
                format!(", {place}")
            };
            if is_dump(invocation) && reaches_transcript(check.model, invocation, Reveals::Dump, 0)
            {
                return Some(message(&format!(
                    "the full environment (`{}`{place})",
                    invocation.shown()
                )));
            }
            if let Some((path, reveals)) = secret_read(invocation, home.as_deref()) {
                if reaches_transcript(check.model, invocation, reveals, 0) {
                    return Some(message(&format!("a known secret file ({path}{place})")));
                }
            }
        }
        let (node, evidence) =
            opaque::evidenced(check.model, &[OpaqueKind::Unparsed], secret_evidence)?;
        Some(message(&format!(
            "the output of code it cannot read ({})",
            opaque::reason(node, &evidence)
        )))
    }
}

fn message(phrase: &str) -> String {
    [
        format!("Refusing to run this command: it would print {phrase} into"),
        "the transcript, where the output persists in session logs that".to_string(),
        "models and users read later.".to_string(),
        String::new(),
        "Read only what you need instead: printenv SAFE_VAR for a single".to_string(),
        "variable, env | grep SAFE_VAR to filter a dump, or grep KEY".to_string(),
        "<file> for one key out of a file.".to_string(),
        String::new(),
        "If the full output is intentional, retry with".to_string(),
        "bash(command, allow_secret_echo=True), or start the kernel with".to_string(),
        "PI_BASH_ALLOW_SECRET_ECHO=1.".to_string(),
    ]
    .join("\n")
}

/// Whether `invocation` prints every variable with its value.
fn is_dump(invocation: &Invocation) -> bool {
    let Some(program) = invocation.program() else {
        return false;
    };
    let args = &invocation.argv[1..];
    if program == "env" {
        // Only options and assignments: `env` prints the environment. `env
        // -i` / `env -` prints only what it is given.
        let mut value = false;
        for arg in args {
            if std::mem::take(&mut value) {
                continue;
            }
            match arg.known() {
                Some("-i" | "-" | "--ignore-environment") => return false,
                Some("-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string") => value = true,
                Some(flag)
                    if flag.starts_with('-') && !flag.starts_with("--") && flag.contains('i') =>
                {
                    return false
                }
                Some(text) if text.starts_with('-') || text.contains('=') => {}
                // A word that may be a command.
                Some(_) | None => return false,
            }
        }
        return true;
    }
    let Some(args): Option<Vec<&str>> = args.iter().map(Arg::known).collect() else {
        return false;
    };
    let flags_only = args.iter().all(|arg| arg.starts_with('-'));
    match program {
        "printenv" => flags_only,
        "export" => {
            flags_only
                && (args.is_empty()
                    || args.iter().any(|flag| {
                        *flag == "-" || flag.chars().skip(1).any(|letter| letter != 'f')
                    }))
        }
        "set" => args.is_empty(),
        "declare" | "typeset" => {
            flags_only
                && !args
                    .iter()
                    .any(|flag| flag.contains('f') || flag.contains('F'))
        }
        _ => false,
    }
}

/// Programs whose output carries their input through to the transcript.
const PASSTHROUGH: [&str; 22] = [
    "cat", "tee", "sort", "uniq", "head", "tail", "less", "more", "column", "nl", "tac", "rev",
    "fold", "fmt", "pr", "paste", "bat", "batcat", "xxd", "od", "hexdump", "tr",
];
/// Filters that still print a secret file's content (a key file's lines
/// are all secret).
const CONTENT_FILTERS: [&str; 9] = [
    "grep", "egrep", "fgrep", "rg", "sed", "awk", "cut", "strings", "base64",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reveals {
    Dump,
    File,
}

/// Programs that print the substitution output they are given.
const PRINTERS: [&str; 4] = ["echo", "printf", "cat", "print"];

/// Whether `invocation`'s stdout ends up in the transcript, unfiltered.
fn reaches_transcript(
    model: &Model,
    invocation: &Invocation,
    reveals: Reveals,
    depth: usize,
) -> bool {
    if depth > 16 {
        return true;
    }
    match &invocation.stdout {
        Output::Transcript => true,
        Output::File(_) => false,
        // Captured output reaches the transcript through a printer, or as
        // the words of a command (`$(env)`): bash runs them and names the
        // first in its "command not found" error.
        Output::Captured => {
            model
                .capturer(invocation)
                .is_some_and(|consumer| match consumer.program() {
                    Some(program) => {
                        PRINTERS.contains(&program)
                            && reaches_transcript(model, consumer, reveals, depth + 1)
                    }
                    None => true,
                })
        }
        Output::Pipe => {
            let Some(consumer) = model.next_stage_head(invocation) else {
                return false;
            };
            let Some(program) = consumer.program() else {
                return false;
            };
            let carries = PASSTHROUGH.contains(&program)
                || (matches!(program, "grep" | "egrep" | "fgrep" | "rg")
                    && !grep_filters(consumer, reveals))
                || (reveals == Reveals::File && CONTENT_FILTERS.contains(&program));
            carries && reaches_transcript(model, consumer, reveals, depth + 1)
        }
    }
}

/// Lines a dump holds that a filter must not pass wholesale.
const SECRET_LINES: [&str; 10] = [
    "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY",
    "GITHUB_TOKEN=ghp_16C7e42F292c6912E7710c838347Ae178B4a",
    "OPENAI_API_KEY=sk-proj-abc123def456",
    "ANTHROPIC_API_KEY=sk-ant-api03-abc123",
    "NPM_TOKEN=npm_abc123def456",
    "DATABASE_PASSWORD=hunter2hunter2",
    "SLACK_BOT_TOKEN=xoxb-123-456-abc",
    "STRIPE_SECRET_KEY=sk_live_abc123",
    "DATABASE_URL=postgres://app:hunter2@db.internal:5432/app",
    "SESSION_TOKEN=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
];

/// What a grep `--max-count` value does to the output.
enum MaxCount {
    /// Not a count: grep exits with a usage error and prints no line.
    Invalid,
    /// At most one line.
    One,
    /// A larger cap: the pattern decides.
    Many,
    /// Zero (BSD grep reads it as no limit) or unknown.
    Unbounded,
}

fn max_count(value: Option<String>) -> MaxCount {
    let Some(value) = value else {
        return MaxCount::Unbounded;
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return MaxCount::Invalid;
    }
    match value.trim_start_matches('0') {
        "" => MaxCount::Unbounded,
        "1" => MaxCount::One,
        _ => MaxCount::Many,
    }
}

/// Whether a grep reading a dump (or a secret file) narrows it: its
/// pattern passes at most one of [`SECRET_LINES`], it does not invert,
/// it prints no context lines, and a zero `--max-count` (which BSD grep
/// reads as no limit) is not taken as a bound. A secret file's lines are all secret, so
/// no grep narrows one.
fn grep_filters(grep: &Invocation, reveals: Reveals) -> bool {
    if reveals == Reveals::File {
        return false;
    }
    let mut reading = match read_grep(grep) {
        Ok(reading) => reading,
        Err(decided) => return decided,
    };
    if reading.at_most_one {
        return true;
    }
    if reading.patterns.is_empty() {
        if reading.operands.is_empty() {
            return false;
        }
        let pattern = reading.operands.remove(0);
        reading.patterns.push(pattern);
    }
    SECRET_LINES
        .iter()
        .filter(|line| reading.passes(line))
        .count()
        <= 1
}

/// How a grep pattern is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Basic,
    Extended,
    Fixed,
}

/// A grep's options and operands, as far as they decide what it prints.
#[derive(Debug)]
struct GrepReading {
    patterns: Vec<String>,
    operands: Vec<String>,
    syntax: Syntax,
    ignore_case: bool,
    only_matching: bool,
    at_most_one: bool,
}

impl GrepReading {
    /// Whether a line passes (with `-o`: whether the match reaches into the
    /// value after `=`).
    fn passes(&self, line: &str) -> bool {
        let value_at = line.find('=').map_or(0, |at| at + 1);
        self.patterns.iter().any(|pattern| {
            if self.syntax == Syntax::Fixed || pattern.is_empty() {
                let (haystack, needle) = if self.ignore_case {
                    (line.to_lowercase(), pattern.to_lowercase())
                } else {
                    (line.to_string(), pattern.clone())
                };
                return haystack
                    .match_indices(&needle)
                    .any(|(at, found)| !self.only_matching || at + found.len() > value_at);
            }
            regex::RegexBuilder::new(&if self.syntax == Syntax::Extended {
                pattern.clone()
            } else {
                basic_to_rust(pattern)
            })
            .case_insensitive(self.ignore_case)
            .size_limit(1 << 20)
            .build()
            .map_or(true, |regex| {
                regex
                    .find_iter(line)
                    .any(|found| !self.only_matching || found.end() > value_at)
            })
        })
    }

    fn set_syntax(&mut self, syntax: Syntax) {
        // `-F` wins over `-E` in either order.
        if self.syntax != Syntax::Fixed {
            self.syntax = syntax;
        }
    }

    /// Apply a max count; `Some(decided)` when it alone decides.
    fn max_count(&mut self, value: Option<String>) -> Option<bool> {
        match max_count(value) {
            MaxCount::Invalid => Some(true),
            MaxCount::One => {
                self.at_most_one = true;
                None
            }
            MaxCount::Many => None,
            MaxCount::Unbounded => Some(false),
        }
    }
}

/// Read `grep`'s argv; `Err(decided)` when an option alone decides whether
/// it filters (an inversion, a count, an unreadable word).
fn read_grep(grep: &Invocation) -> Result<GrepReading, bool> {
    let mut reading = GrepReading {
        patterns: Vec::new(),
        operands: Vec::new(),
        syntax: if grep.program() == Some("egrep") {
            Syntax::Extended
        } else {
            Syntax::Basic
        },
        ignore_case: false,
        only_matching: false,
        at_most_one: false,
    };
    let mut args = grep.argv[1..].iter();
    let mut options_done = false;
    while let Some(arg) = args.next() {
        let Some(text) = arg.known() else {
            return Err(false);
        };
        if options_done || !text.starts_with('-') || text == "-" {
            reading.operands.push(text.to_string());
            continue;
        }
        if text == "--" {
            options_done = true;
            continue;
        }
        let decided = match text.strip_prefix("--") {
            Some(long) => long_grep_option(long, &mut args, &mut reading),
            None => short_grep_options(&text[1..], &mut args, &mut reading),
        };
        if let Some(decided) = decided {
            return Err(decided);
        }
    }
    Ok(reading)
}

/// One `--option[=value]` of grep; `Some(decided)` when it alone decides.
fn long_grep_option(
    long: &str,
    args: &mut std::slice::Iter<'_, Arg>,
    reading: &mut GrepReading,
) -> Option<bool> {
    let (name, value) = match long.split_once('=') {
        Some((name, value)) => (name, Some(value.to_string())),
        None => (long, None),
    };
    let mut take = || {
        value
            .clone()
            .or_else(|| args.next().and_then(Arg::known).map(str::to_string))
    };
    let is = |option: &str| option.starts_with(name) && name.len() >= 3;
    if is("invert-match") || is("null-data") || is("file") {
        return Some(false);
    }
    if [
        "count",
        "files-with-matches",
        "files-without-match",
        "quiet",
        "silent",
    ]
    .into_iter()
    .any(is)
    {
        return Some(true);
    }
    if is("only-matching") {
        reading.only_matching = true;
    } else if is("regexp") {
        reading.patterns.push(take().unwrap_or_default());
    } else if is("fixed-strings") {
        reading.set_syntax(Syntax::Fixed);
    } else if is("ignore-case") {
        reading.ignore_case = true;
    } else if is("extended-regexp") {
        reading.set_syntax(Syntax::Extended);
    } else if ["after-context", "before-context", "context"]
        .into_iter()
        .any(is)
    {
        if take()
            .and_then(|count| count.parse::<u32>().ok())
            .is_none_or(|count| count > 0)
        {
            return Some(false);
        }
    } else if "max-count".starts_with(name) && name.len() >= 2 {
        // `--ma` is already unambiguous to getopt.
        return reading.max_count(take());
    }
    None
}

/// A cluster of short grep options (`letters` after the `-`);
/// `Some(decided)` when one alone decides.
fn short_grep_options(
    letters: &str,
    args: &mut std::slice::Iter<'_, Arg>,
    reading: &mut GrepReading,
) -> Option<bool> {
    let letters: Vec<char> = letters.chars().collect();
    let mut digits = String::new();
    for (at, letter) in letters.iter().enumerate() {
        let glued: String = letters[at + 1..].iter().collect();
        let mut value = || {
            if glued.is_empty() {
                args.next().and_then(Arg::known).map(str::to_string)
            } else {
                Some(glued.clone())
            }
        };
        match letter {
            'v' | 'z' | 'f' => return Some(false),
            // Counts, file names, a status: no line is printed.
            'c' | 'l' | 'L' | 'q' => return Some(true),
            'o' => reading.only_matching = true,
            'F' => reading.set_syntax(Syntax::Fixed),
            'i' | 'y' => reading.ignore_case = true,
            'E' => reading.set_syntax(Syntax::Extended),
            'e' => {
                reading.patterns.push(value().unwrap_or_default());
                break;
            }
            'm' => {
                if let Some(decided) = reading.max_count(value()) {
                    return Some(decided);
                }
                break;
            }
            'A' | 'B' | 'C' => {
                if value()
                    .and_then(|count| count.parse::<u32>().ok())
                    .is_none_or(|count| count > 0)
                {
                    return Some(false);
                }
                break;
            }
            digit if digit.is_ascii_digit() => digits.push(*digit),
            _ => {}
        }
    }
    digits
        .parse::<u32>()
        .is_ok_and(|count| count > 0)
        .then_some(false)
}

/// A grep pattern (basic or extended) as a Rust regex: in a basic
/// expression `\|`, `\(`, `\)`, `\{`, `\}`, `\+`, `\?` are the operators.
fn basic_to_rust(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(next @ ('|' | '(' | ')' | '{' | '}' | '+' | '?')) => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push_str("\\\\"),
            }
        } else if matches!(ch, '|' | '(' | ')' | '{' | '}' | '+' | '?') {
            out.push('\\');
            out.push(ch);
        } else {
            out.push(ch);
        }
    }
    out
}

const READERS: [&str; 16] = [
    "cat", "tac", "nl", "head", "tail", "less", "more", "bat", "batcat", "xxd", "od", "hexdump",
    "base64", "strings", "pr", "fold",
];

/// The secret file `invocation` prints, if any, and how a filter reads
/// it (a process environment filters like a dump).
fn secret_read(invocation: &Invocation, home: Option<&Path>) -> Option<(String, Reveals)> {
    let program = invocation.program()?;
    if !READERS.contains(&program) {
        return None;
    }
    let mut skip = false;
    for arg in &invocation.argv[1..] {
        if skip {
            skip = false;
            continue;
        }
        if let Some(text) = arg.known() {
            if text.starts_with('-') && text.len() > 1 {
                skip = matches!(text, "-n" | "-c" | "--lines" | "--bytes");
                continue;
            }
        }
        let shown = arg.shown();
        let candidates: Vec<String> = match arg {
            Arg::Known(text) => with_dirs(invocation, text),
            Arg::Pattern(text) => secret_spellings(text)
                .iter()
                .flat_map(|spelling| with_dirs(invocation, spelling))
                .collect(),
            Arg::Unknown(unknown) => vec![format!("{}\u{0}", unknown.prefix)],
        };
        if let Some(path) = candidates.iter().find(|path| is_secret_path(path, home)) {
            let reveals = if path.ends_with("environ") {
                Reveals::Dump
            } else {
                Reveals::File
            };
            return Some((shown, reveals));
        }
    }
    None
}

/// `text` as given and joined to each directory the command may run in.
fn with_dirs(invocation: &Invocation, text: &str) -> Vec<String> {
    invocation
        .cwd
        .dirs
        .iter()
        .map(|dir| join(dir, text).to_string_lossy().into_owned())
        .chain(std::iter::once(text.to_string()))
        .collect()
}

/// The names [`is_secret_path`] looks for, which a glob component may match.
const SECRET_NAMES: [&str; 33] = [
    ".ssh",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    ".aws",
    "credentials",
    "sso",
    "cli",
    ".gnupg",
    "private-keys-v1.d",
    "secring.gpg",
    ".anthropic-accounts",
    "proc",
    "self",
    "environ",
    ".netrc",
    ".git-credentials",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".docker",
    "config.json",
    ".config",
    "gh",
    "hosts.yml",
    ".kube",
    "config",
    ".cargo",
    "credentials.toml",
    ".prime",
    "agent",
    "auth.json",
];

/// The spellings of a glob that matched no file when judged: the pattern
/// itself (bash passes it on literally), and each path it would match had
/// the secret files existed (`~/.aws/cred*` reads `~/.aws/credentials`).
fn secret_spellings(pattern: &str) -> Vec<String> {
    const CAP: usize = 64;
    let mut spellings = vec![String::new()];
    for (at, component) in pattern.split('/').enumerate() {
        let names: Vec<&str> = if component.contains(['*', '?', '[']) {
            SECRET_NAMES
                .iter()
                .copied()
                .filter(|name| {
                    (!name.starts_with('.') || component.starts_with('.'))
                        && pattern_matches(component, name)
                })
                .chain(std::iter::once(component))
                .collect()
        } else {
            vec![component]
        };
        spellings = spellings
            .iter()
            .flat_map(|head| {
                names.iter().map(move |name| {
                    if at == 0 {
                        (*name).to_string()
                    } else {
                        format!("{head}/{name}")
                    }
                })
            })
            .take(CAP)
            .collect();
    }
    spellings
}

/// Whether `path` names a secret: a private key under `.ssh`, AWS
/// credentials, a `GnuPG` private key, a token file, a process environment.
/// A trailing NUL marks a path whose end is only known at run time.
fn is_secret_path(path: &str, home: Option<&Path>) -> bool {
    let open_end = path.ends_with('\u{0}');
    let path = path.trim_end_matches('\u{0}');
    let names: Vec<String> = Path::new(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            Component::RootDir
            | Component::CurDir
            | Component::ParentDir
            | Component::Prefix(_) => None,
        })
        .collect();
    let last = names.last().map(String::as_str).unwrap_or_default();
    // With HOME known, a `~` or `$` still in a resolved path was quoted
    // (`'~/.ssh/id_rsa'`, `\$HOME/.aws/credentials`): bash reads a
    // directory literally named so, not the secret it spells.
    if home.is_some()
        && names
            .iter()
            .any(|name| name.starts_with('~') || name.contains('$'))
    {
        return false;
    }
    for (at, name) in names.iter().enumerate() {
        let rest = &names[at + 1..];
        match name.as_str() {
            ".ssh" => {
                let Some(file) = rest.first() else {
                    return open_end && path.ends_with('/');
                };
                let public = Path::new(file)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("pub"))
                    || matches!(
                        file.as_str(),
                        "config"
                            | "known_hosts"
                            | "known_hosts.old"
                            | "authorized_keys"
                            | "authorized_keys2"
                            | "environment"
                            | "rc"
                    )
                    || file.starts_with("config");
                return !public || (open_end && rest.len() == 1 && path.ends_with('/'));
            }
            ".aws" => {
                return rest
                    .first()
                    .is_some_and(|file| file == "credentials" || file == "sso" || file == "cli")
                    || (open_end && rest.is_empty());
            }
            ".gnupg" => {
                return rest.iter().any(|part| {
                    part.contains("private-keys")
                        || part == "secring.gpg"
                        || part.contains("revocs")
                }) || Path::new(last)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("key"))
                    || (open_end && rest.is_empty());
            }
            ".anthropic-accounts" => return true,
            // `/proc/<pid>/environ` and `/proc/<pid>/task/<tid>/environ`.
            "environ"
                if at + 1 == names.len()
                    && names[0] == "proc"
                    && (at == 2 || (at == 4 && names[2] == "task")) =>
            {
                return true;
            }
            _ => {}
        }
    }
    let tail = |parts: &[&str]| {
        names.len() >= parts.len()
            && names[names.len() - parts.len()..]
                .iter()
                .zip(parts)
                .all(|(a, b)| a == b)
    };
    let in_home = home.is_some_and(|home| Path::new(path).parent() == Some(home));
    (in_home
        && matches!(
            last,
            ".netrc" | ".git-credentials" | ".pgpass" | ".npmrc" | ".pypirc"
        ))
        || tail(&[".docker", "config.json"])
        || tail(&[".config", "gh", "hosts.yml"])
        || tail(&[".kube", "config"])
        || tail(&[".cargo", "credentials"])
        || tail(&[".cargo", "credentials.toml"])
        || tail(&[".prime", "agent", "auth.json"])
}

/// An opaque node naming an environment dump or a secret path.
fn secret_evidence(text: &str) -> Option<String> {
    for word in ["env", "printenv"] {
        if find_command_word(text, word) {
            return Some(format!("`{word}`"));
        }
    }
    text.split(|ch: char| ch.is_whitespace() || "'\"`;|&()<>".contains(ch))
        .find(|token| token.contains('/') && is_secret_path(token, None))
        .map(|token| format!("`{token}`"))
}
