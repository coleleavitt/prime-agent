//! The shared classifier of code searches and in-place edits, over a
//! command's words: the `grep` and `sed` guards, the session's steering
//! reminders and the Claude Code `PreToolUse` hook all ask it, so they agree
//! on what a structural search is.
//!
//! Shapes come from the 2026-11-15 corpus study (49,066 grep/rg/sed commands
//! across prime-agent and Claude Code sessions): a grep whose every
//! alternative is a definition (`fn x`, `class X`, `struct X`) or a call
//! site (`name(`) over project source drew the user's structural corrections
//! at 3.8 times the baseline rate; pipe filters, counts and file lists, logs
//! and data, and plain-text searches did not. `sed -i` on source broke the
//! build three to four times as often as an edit-tool write, and skips the
//! /rewind checkpoint.

use std::sync::OnceLock;

use regex::Regex;

/// What a structural search looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchShape {
    /// A definition: `fn name`, `class Name`, `struct Name`, `impl Name`.
    Definition,
    /// The call sites of a function: `name(`, `obj.method(`.
    CallSite,
}

/// A grep invocation's parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one command line, not a state machine"
)]
pub struct GrepArgs {
    /// The patterns searched (`-e` repeats; else the first operand).
    pub patterns: Vec<String>,
    /// The files and directories searched.
    pub paths: Vec<String>,
    /// File type filters (`rg -t rust`, `ag --rust`).
    pub types: Vec<String>,
    /// File globs (`rg -g '*.rs'`, `grep --include=*.py`).
    pub globs: Vec<String>,
    /// Counts or lists files instead of printing lines (`-c`, `-l`, `-L`,
    /// `-q`, `rg --files`).
    pub counts_or_lists: bool,
    /// Searches directories (`rg`, `ag`, `git grep`, `grep -r`).
    pub recursive: bool,
    /// Prints only the matched part (`-o`): bounded output.
    pub only_matching: bool,
    /// Caps the printed columns (`rg -M` / `--max-columns`).
    pub column_cap: bool,
    /// Lists matching files (`-l`, `-L`): with a definition pattern that is
    /// still "where is it defined".
    pub lists_files: bool,
    /// Matches whole words (`-w`): with a code identifier, a references
    /// lookup.
    pub word: bool,
}

/// A grep that looks up code structure in project source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSearch {
    pub shape: SearchShape,
    /// The pattern that made it structural.
    pub pattern: String,
    pub args: GrepArgs,
}

/// The extensions of source files.
pub const SOURCE_EXTENSIONS: [&str; 26] = [
    "rs", "py", "pyi", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "c", "h", "cc", "cpp",
    "hpp", "go", "java", "kt", "swift", "cs", "rb", "php", "dart", "zig", "lua",
];

/// `rg -t` / `ag --<type>` names of source languages.
const SOURCE_TYPES: [&str; 22] = [
    "rust",
    "rs",
    "py",
    "python",
    "ts",
    "typescript",
    "js",
    "javascript",
    "c",
    "cpp",
    "cc",
    "go",
    "java",
    "kotlin",
    "swift",
    "csharp",
    "cs",
    "ruby",
    "php",
    "dart",
    "zig",
    "lua",
];

/// Path fragments that mark code outside the project (dependencies, build
/// output, scratch): searching there is reading, not structure lookup.
const OUTSIDE_FRAGMENTS: [&str; 14] = [
    "/tmp/",
    "/usr/",
    "/opt/",
    "/proc/",
    "node_modules",
    "vendor/",
    ".cargo/registry",
    ".cargo/git",
    "target/",
    ".rustup/",
    "site-packages",
    ".venv/",
    "dist/",
    "scratchpad",
];

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

/// Whether `path` is a source file (a minified `.min.js` bundle is not).
#[must_use]
pub fn is_source_file(path: &str) -> bool {
    !path.contains(".min.")
        && extension(path)
            .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Whether `path` names code outside the project (a dependency, build
/// output, scratch).
#[must_use]
pub fn outside_project(path: &str) -> bool {
    let probe = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    OUTSIDE_FRAGMENTS.iter().any(|fragment| {
        let fragment = fragment.trim_start_matches('/');
        probe.contains(&format!("/{fragment}"))
    })
}

/// Whether `path` is a minified or bundled JavaScript file.
#[must_use]
pub fn is_bundle(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    matches!(extension(&lower), Some("js" | "mjs" | "cjs"))
        && (lower.contains(".min.")
            || lower.contains("chunk")
            || lower.contains("bundle")
            || lower.contains("/dist/")
            || lower.contains("/assets/")
            || lower.ends_with("cli.js"))
}

/// Whether `path` is a scratch location an in-place edit may touch freely.
#[must_use]
pub fn is_scratch(path: &str) -> bool {
    path.starts_with("/tmp/") || path.starts_with("/dev/shm/") || path.contains("scratchpad")
}

fn definition() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^(?:pub(?:\([a-z:]+\))? )?(?:(?:async|unsafe|const|export|default|extern|static|abstract|final|private|protected|public) )*(?:(?:fn|def|class|struct|enum|trait|impl|interface|func|function|mod|type|union) [<A-Za-z_\[(\\.*]|impl<|macro_rules! )",
        )
        .expect("the definition pattern is a valid regex")
    })
}

fn call_site() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(?:\.|::)?[A-Za-z_]\w*(?:(?:::|\.|->)[A-Za-z_]\w*)*\($")
            .expect("the call-site pattern is a valid regex")
    })
}

/// One alternative of a pattern with regex spacing and anchors folded:
/// `\s+` / `\s*` / `[[:space:]]+` become a space, `^`, `\b`, `$` drop, and
/// an escaped `\(` or `\.` is unescaped.
fn normalized(alternative: &str) -> String {
    let mut text = alternative.to_string();
    for (from, to) in [
        ("[[:space:]]+", " "),
        ("[[:space:]]*", " "),
        ("\\s+", " "),
        ("\\s*", " "),
        ("\\s", " "),
        ("\\b", ""),
        ("\\<", ""),
        ("\\>", ""),
        ("\\(", "("),
        ("\\.", "."),
        ("\\$", ""),
    ] {
        text = text.replace(from, to);
    }
    let text = text.trim_start_matches('^').trim_end_matches('$');
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The pattern's alternatives (`a|b`, and basic grep's `a\|b`), outer
/// group parentheses dropped.
fn alternatives(pattern: &str) -> Vec<String> {
    let unified = pattern.replace("\\|", "|");
    let unified = unified
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .filter(|inner| !inner.contains(['(', ')']))
        .unwrap_or(&unified)
        .to_string();
    unified
        .split('|')
        .map(str::trim)
        .filter(|alternative| !alternative.is_empty())
        .map(str::to_string)
        .collect()
}

/// Words a definition alternative starts with that are not its name.
const DEFINITION_WORDS: [&str; 26] = [
    "pub",
    "crate",
    "super",
    "self",
    "in",
    "async",
    "unsafe",
    "const",
    "export",
    "default",
    "extern",
    "static",
    "abstract",
    "final",
    "private",
    "protected",
    "public",
    "fn",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "impl",
    "interface",
    "func",
];

fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| {
            word.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
        })
}

/// The name a structural alternative is about: a definition's first
/// non-keyword word, a call's last path segment.
fn symbol(text: &str, shape: SearchShape) -> Option<&str> {
    match shape {
        SearchShape::Definition => identifiers(text).find(|word| {
            !DEFINITION_WORDS.contains(word)
                && !matches!(*word, "function" | "mod" | "type" | "union" | "macro_rules")
        }),
        SearchShape::CallSite => identifiers(text.trim_end_matches('(')).last(),
    }
}

fn code_path() -> &'static Regex {
    static PATH: OnceLock<Regex> = OnceLock::new();
    PATH.get_or_init(|| {
        Regex::new(r"^(?:\.|::)?[A-Za-z_]\w*(?:(?:::|\.|->)[A-Za-z_]\w*)*(?:::|\()?$")
            .expect("the path pattern is a valid regex")
    })
}

/// Whether `text` is a plain use of one of `symbols`: an identifier or a
/// path (`a.b`, `A::b`, `x->y`) that names one of them.
fn uses_symbol(text: &str, symbols: &[&str]) -> bool {
    code_path().is_match(text) && identifiers(text).any(|word| symbols.contains(&word))
}

/// Whether `text` is shaped like a code identifier rather than a word of
/// prose: `snake_case`, `SCREAMING_CASE`, `camelCase`, `PascalCase` with
/// two capitals, or a path (`a::b`, `mod::`, `x.y`, `p->q`, `name(`).
/// `TODO`, `resolve` or `error` stay text.
///
/// # Panics
///
/// Never: the patterns are constants.
#[must_use]
pub fn is_code_identifier(text: &str) -> bool {
    static CAMEL: OnceLock<Regex> = OnceLock::new();
    let camel = CAMEL.get_or_init(|| {
        Regex::new(r"[a-z0-9][A-Z]|^[A-Z][a-z0-9]+[A-Z]")
            .expect("the camel pattern is a valid regex")
    });
    code_path().is_match(text)
        && (text.trim_matches('_').contains('_')
            || text.contains("::")
            || text.contains('.')
            || text.contains("->")
            || text.ends_with('(')
            || camel.is_match(text))
}

/// The structural shape of `pattern`: at least one alternative is a
/// definition or a call site, and every other alternative is one too or a
/// plain use of a name those alternatives are about
/// (`fn cluster_dobs|cluster_dobs.insert|cluster_dobs`: a definition and its
/// uses), or another code identifier (`BOOK_REFRESH_CHAIN`, `log_text::`).
/// A definition wins over a call site.
#[must_use]
pub fn pattern_shape(pattern: &str) -> Option<SearchShape> {
    let alternatives: Vec<String> = alternatives(pattern)
        .iter()
        .map(|alternative| normalized(alternative))
        .collect();
    let mut shape = None;
    let mut symbols: Vec<&str> = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    for text in &alternatives {
        let found = if definition().is_match(text) {
            Some(SearchShape::Definition)
        } else if call_site().is_match(text) {
            Some(SearchShape::CallSite)
        } else {
            None
        };
        match found {
            Some(found) => {
                symbols.extend(symbol(text, found));
                if shape != Some(SearchShape::Definition) {
                    shape = Some(found);
                }
            }
            None => plain.push(text),
        }
    }
    let shape = shape?;
    plain
        .iter()
        .all(|text| uses_symbol(text, &symbols) || is_code_identifier(text))
        .then_some(shape)
}

/// The value a flag takes from the next word (`-e PAT`, `-t rust`), by
/// program.
fn takes_value(program: &str, flag: &str) -> bool {
    match program {
        "rg" => matches!(
            flag,
            "-e" | "--regexp"
                | "-f"
                | "--file"
                | "-t"
                | "--type"
                | "-T"
                | "--type-not"
                | "-g"
                | "--glob"
                | "--iglob"
                | "-A"
                | "-B"
                | "-C"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "-m"
                | "--max-count"
                | "-M"
                | "--max-columns"
                | "-d"
                | "--max-depth"
                | "-j"
                | "--threads"
                | "-E"
                | "--encoding"
                | "-r"
                | "--replace"
                | "--type-add"
                | "--sort"
                | "--sortr"
                | "--color"
                | "--colors"
                | "--pre"
                | "--ignore-file"
                | "--context-separator"
                | "--field-match-separator"
        ),
        "ag" => matches!(
            flag,
            "-G" | "--file-search-regex"
                | "--ignore"
                | "-A"
                | "-B"
                | "-C"
                | "-m"
                | "--max-count"
                | "--depth"
        ),
        _ => matches!(
            flag,
            "-e" | "--regexp"
                | "-f"
                | "--file"
                | "-A"
                | "-B"
                | "-C"
                | "--after-context"
                | "--before-context"
                | "--context"
                | "-m"
                | "--max-count"
                | "-d"
                | "--directories"
                | "-D"
                | "--devices"
                | "--label"
                | "--color"
                | "--colour"
                | "--include"
                | "--exclude"
                | "--exclude-dir"
                | "--max-depth"
        ),
    }
}

fn counts_or_lists(flag: &str) -> bool {
    matches!(
        flag,
        "--count"
            | "--count-matches"
            | "--files-with-matches"
            | "--files-without-match"
            | "--files"
            | "--quiet"
            | "--name-only"
            | "--type-list"
    )
}

/// The parts of a grep-like invocation: `program` is `rg`, `grep`,
/// `egrep`, `fgrep`, `ag` or `git-grep` (git's subcommand, after `grep`);
/// `None` for another program.
#[must_use]
pub fn parse_grep(program: &str, args: &[&str]) -> Option<GrepArgs> {
    // `git grep` searches the work tree like `grep -r`.
    let recursive = matches!(program, "rg" | "ag" | "git-grep");
    let program = match program {
        "rg" | "ag" => program,
        "grep" | "egrep" | "fgrep" | "git-grep" => "grep",
        _ => return None,
    };
    let mut out = GrepArgs {
        recursive,
        ..GrepArgs::default()
    };
    let mut pattern_given = false;
    let mut operands: Vec<&str> = Vec::new();
    let mut index = 0;
    let mut flags_done = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if flags_done || !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
            continue;
        }
        if arg == "--" {
            flags_done = true;
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg, None),
        };
        if let Some(long) = flag.strip_prefix("--") {
            if counts_or_lists(flag) {
                out.counts_or_lists = true;
            }
            match long {
                "recursive" | "dereference-recursive" => out.recursive = true,
                "only-matching" => out.only_matching = true,
                "files-with-matches" | "files-without-match" => out.lists_files = true,
                "word-regexp" => out.word = true,
                _ => {}
            }
            if program == "ag" && SOURCE_TYPES.contains(&long) {
                out.types.push(long.to_string());
            }
            if takes_value(program, flag) {
                let value = inline.or_else(|| {
                    let value = args.get(index).map(|value| (*value).to_string());
                    index += 1;
                    value
                });
                apply_value(&mut out, program, flag, value, &mut pattern_given);
            }
            continue;
        }
        // A short-flag bundle: `-rn`, `-A3`, `-tpy`, `-e PAT`.
        let letters: Vec<char> = flag[1..].chars().collect();
        let mut at = 0;
        while at < letters.len() {
            let letter = letters[at];
            let short = format!("-{letter}");
            at += 1;
            match letter {
                'c' | 'q' => out.counts_or_lists = true,
                'l' | 'L' => {
                    out.counts_or_lists = true;
                    out.lists_files = true;
                }
                'w' => out.word = true,
                'r' | 'R' if program == "grep" => out.recursive = true,
                'o' => out.only_matching = true,
                _ => {}
            }
            if takes_value(program, &short) {
                let rest: String = letters[at..].iter().collect();
                let value = if rest.is_empty() {
                    let value = args.get(index).map(|value| (*value).to_string());
                    index += 1;
                    value
                } else {
                    Some(rest)
                };
                apply_value(&mut out, program, &short, value, &mut pattern_given);
                break;
            }
        }
    }
    let mut operands = operands.into_iter();
    if !pattern_given {
        out.patterns.extend(operands.next().map(str::to_string));
    }
    out.paths = operands.map(str::to_string).collect();
    Some(out)
}

fn apply_value(
    out: &mut GrepArgs,
    program: &str,
    flag: &str,
    value: Option<String>,
    pattern_given: &mut bool,
) {
    let Some(value) = value else {
        return;
    };
    match flag {
        "-e" | "--regexp" => {
            out.patterns.push(value);
            *pattern_given = true;
        }
        "-f" | "--file" => *pattern_given = true,
        "-t" | "--type" if program == "rg" => out.types.push(value),
        "-g" | "--glob" | "--iglob" | "--include" => out.globs.push(value),
        "-M" | "--max-columns" if program == "rg" => out.column_cap = true,
        "-d" | "--directories" if program == "grep" && value == "recurse" => out.recursive = true,
        _ => {}
    }
}

fn glob_is_source(glob: &str) -> bool {
    let tail = glob.rsplit('/').next().unwrap_or(glob);
    if let Some(alternatives) = tail
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(inner, _)| inner)
    {
        return alternatives
            .split(',')
            .any(|ext| SOURCE_EXTENSIONS.contains(&ext.trim_start_matches('.')));
    }
    is_source_file(tail)
}

/// Whether a search with `args` reads project source: a source type or
/// glob, a source file or a directory operand inside the project, or a
/// recursive search of the working directory. Any operand outside the
/// project, or only data files, makes it not.
#[must_use]
pub fn targets_project_source(args: &GrepArgs) -> bool {
    if args.paths.iter().any(|path| outside_project(path)) {
        return false;
    }
    if !args.types.is_empty() {
        return args
            .types
            .iter()
            .any(|kind| SOURCE_TYPES.contains(&kind.to_ascii_lowercase().as_str()));
    }
    if !args.globs.is_empty() && args.globs.iter().all(|glob| !glob.starts_with('!')) {
        return args.globs.iter().any(|glob| glob_is_source(glob));
    }
    if args.paths.is_empty() {
        return args.recursive;
    }
    args.paths.iter().any(|path| {
        is_source_file(path)
            || (extension(path).is_none() && (args.recursive || path.ends_with('/')))
    })
}

/// The structural search `program args` runs, when it is one: every
/// pattern a definition or call-site shape, over project source, printing
/// lines (not counting or listing), and not a filter of piped input.
#[must_use]
pub fn structural_grep(
    program: &str,
    args: &[&str],
    piped_input: bool,
) -> Option<StructuralSearch> {
    let args = parse_grep(program, args)?;
    if (piped_input && args.paths.is_empty()) || (args.counts_or_lists && !args.lists_files) {
        return None;
    }
    let mut shape = None;
    for pattern in &args.patterns {
        let found = pattern_shape(pattern).or_else(|| {
            // `-w snake_case_name`: every use of a code identifier.
            (args.word
                && alternatives(pattern)
                    .iter()
                    .all(|alternative| is_code_identifier(&normalized(alternative))))
            .then_some(SearchShape::CallSite)
        })?;
        if shape != Some(SearchShape::Definition) {
            shape = Some(found);
        }
    }
    let shape = shape?;
    // `-l` lists files: "which file defines X" is structural, "which files
    // mention X" is not.
    if args.lists_files && shape != SearchShape::Definition {
        return None;
    }
    targets_project_source(&args).then(|| StructuralSearch {
        shape,
        pattern: args.patterns.join("|"),
        args,
    })
}

/// A grep over minified bundles without bounded output (`-o`, `-c`, `-l`,
/// a column cap): one matching line can be megabytes.
#[must_use]
pub fn unbounded_bundle_grep(program: &str, args: &[&str]) -> bool {
    parse_grep(program, args).is_some_and(|args| {
        !args.counts_or_lists
            && !args.only_matching
            && !args.column_cap
            && args.paths.iter().any(|path| is_bundle(path))
    })
}

/// An in-place `sed` edit and the files it names (empty when they are not
/// literal words, e.g. fed by `xargs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedInPlace {
    pub files: Vec<String>,
}

/// The in-place edit `sed args` makes, when it makes one.
#[must_use]
pub fn sed_in_place(args: &[&str]) -> Option<SedInPlace> {
    let mut in_place = false;
    let mut script_given = false;
    let mut operands: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if arg == "--" {
            operands.extend(&args[index..]);
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
            continue;
        }
        if arg == "--in-place" || arg.starts_with("--in-place=") {
            in_place = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if matches!(long, "expression" | "file") {
                script_given = true;
                index += 1;
            } else if long.starts_with("expression=") || long.starts_with("file=") {
                script_given = true;
            }
            continue;
        }
        // Short bundle: `-i`, `-i.bak`, `-Ei`, `-ni`, `-e SCRIPT`.
        let letters: Vec<char> = arg[1..].chars().collect();
        for (at, letter) in letters.iter().enumerate() {
            match letter {
                // `-i` swallows the rest of the bundle as its suffix.
                'i' => {
                    in_place = true;
                    break;
                }
                'e' | 'f' => {
                    script_given = true;
                    if at + 1 == letters.len() {
                        index += 1;
                    }
                    break;
                }
                'l' => {
                    if at + 1 == letters.len() {
                        index += 1;
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    if !in_place {
        return None;
    }
    let files = operands
        .into_iter()
        .skip(usize::from(!script_given))
        .map(str::to_string)
        .collect();
    Some(SedInPlace { files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<&str> {
        command.split_whitespace().collect()
    }

    fn structural(command: &str, piped: bool) -> Option<SearchShape> {
        let words = words(command);
        let (program, args) = match words.as_slice() {
            ["git", "grep", rest @ ..] => ("git-grep", rest.to_vec()),
            [program, rest @ ..] => (*program, rest.to_vec()),
            [] => return None,
        };
        structural_grep(program, &args, piped).map(|found| found.shape)
    }

    #[test]
    fn definition_and_call_lookups_over_source_are_structural() {
        for (command, shape) in [
            ("rg -n fn\\s+send_now crates/", SearchShape::Definition),
            ("rg -n ^pub\\s+fn\\s+check -t rust", SearchShape::Definition),
            ("grep -rn class\\s+Session src/", SearchShape::Definition),
            (
                "rg struct\\s+Model|enum\\s+Arg crates/pa-bash",
                SearchShape::Definition,
            ),
            ("git grep -n impl\\s+Rule", SearchShape::Definition),
            (
                "rg -n isolate_hook\\( crates/pa-core",
                SearchShape::CallSite,
            ),
            ("rg -n \\.send_now\\( -g *.rs", SearchShape::CallSite),
            (
                "grep -rn --include=*.py def\\s+_run_guarded .",
                SearchShape::Definition,
            ),
            ("rg -tpy def\\s+bash", SearchShape::Definition),
            ("rg -l fn\\s+send_now crates/", SearchShape::Definition),
            ("rg -nw cluster_dobs crates/", SearchShape::CallSite),
            (
                "rg -n str_array\\(|fn\\s+strings\\(|log_text::|inject_trace_context\\( crates src",
                SearchShape::Definition,
            ),
            ("rg fn\\s+check src/pipeline.rs", SearchShape::Definition),
        ] {
            assert_eq!(structural(command, false), Some(shape), "{command}");
        }
    }

    #[test]
    fn filters_counts_text_logs_and_dependencies_are_not() {
        for (command, piped) in [
            ("grep fn\\s+main", true),
            ("rg -c isolate_hook\\( crates/", false),
            ("rg -n TODO crates/", false),
            ("rg -n connection\\s+refused logs/", false),
            ("rg -n fn\\s+send_now agent.jsonl", false),
            ("rg -n fn\\s+serialize ~/.cargo/registry/src", false),
            ("rg -n fn\\s+parse /tmp/scratch", false),
            ("grep -n handle_request notes.md", false),
            ("rg -n fn\\s+main -t md", false),
            ("grep fn\\s+main", false),
            ("rg -n x_y_z crates/", false),
            ("rg -l isolate_hook\\( crates/", false),
            ("rg -nw TODO crates/", false),
            (
                "rg -n parse_orion_instant|edited_since_watermark crates/x.rs",
                false,
            ),
        ] {
            assert_eq!(structural(command, piped), None, "{command}");
        }
    }

    #[test]
    fn grep_arguments_parse_across_spellings() {
        let args = parse_grep(
            "rg",
            &[
                "-nA3",
                "-e",
                "fn x",
                "--glob=*.rs",
                "-tpy",
                "src",
                "--",
                "-weird",
            ],
        )
        .unwrap();
        assert_eq!(args.patterns, ["fn x"]);
        assert_eq!(args.globs, ["*.rs"]);
        assert_eq!(args.types, ["py"]);
        assert_eq!(args.paths, ["src", "-weird"]);
        let args = parse_grep("grep", &["-rnE", "a|b", "--include", "*.ts", "web"]).unwrap();
        assert!(args.recursive);
        assert_eq!(args.patterns, ["a|b"]);
        assert_eq!(args.globs, ["*.ts"]);
        assert!(parse_grep("sed", &["x"]).is_none());
    }

    #[test]
    fn pattern_shapes() {
        assert_eq!(pattern_shape("fn foo"), Some(SearchShape::Definition));
        assert_eq!(
            pattern_shape("pub(crate) fn foo"),
            Some(SearchShape::Definition)
        );
        assert_eq!(pattern_shape("impl<T> Foo"), Some(SearchShape::Definition));
        assert_eq!(
            pattern_shape("async def handler"),
            Some(SearchShape::Definition)
        );
        assert_eq!(pattern_shape("Foo::new\\("), Some(SearchShape::CallSite));
        assert_eq!(pattern_shape("foo(|bar("), Some(SearchShape::CallSite));
        assert_eq!(
            pattern_shape("fn foo|TODO"),
            None,
            "an unrelated alternative keeps it a text search"
        );
        // A definition and the uses of the same name (the live 2026-11-15
        // ers-rs miss).
        assert_eq!(
            pattern_shape("fn cluster_dobs|cluster_dobs.insert|cluster_dobs"),
            Some(SearchShape::Definition)
        );
        assert_eq!(
            pattern_shape("Self::cluster_dobs|fn\\s+cluster_dobs"),
            Some(SearchShape::Definition)
        );
        assert_eq!(
            pattern_shape("send_now\\(|\\.send_now"),
            Some(SearchShape::CallSite)
        );
        assert_eq!(pattern_shape("fn foo|bar"), None, "a prose word");
        assert_eq!(
            pattern_shape(
                "BOOK_REFRESH_CHAIN|refresh_book_chain|fn run_auto_resolve|auto_resolve\\("
            ),
            Some(SearchShape::Definition),
            "other code identifiers ride along"
        );
        assert!(is_code_identifier("log_text::") && is_code_identifier("sendQueuedNow"));
        assert!(
            !is_code_identifier("TODO")
                && !is_code_identifier("resolve")
                && !is_code_identifier("Error")
        );
        assert_eq!(
            pattern_shape("cluster_dobs"),
            None,
            "a bare name alone stays text"
        );
        assert_eq!(pattern_shape("error: could not compile"), None);
        assert_eq!(pattern_shape("X-Request-Id"), None);
        assert_eq!(pattern_shape("function"), None);
        assert_eq!(pattern_shape("fnord"), None, "a keyword needs its space");
    }

    #[test]
    fn in_place_sed_and_its_files() {
        assert_eq!(
            sed_in_place(&["-i", "s/a/b/", "src/lib.rs"]),
            Some(SedInPlace {
                files: vec!["src/lib.rs".to_string()]
            })
        );
        assert_eq!(
            sed_in_place(&["-i.bak", "-e", "s/a/b/", "a.py", "b.py"]),
            Some(SedInPlace {
                files: vec!["a.py".to_string(), "b.py".to_string()]
            })
        );
        assert_eq!(
            sed_in_place(&["-Ei", "s/a/b/g", "x.ts"]).unwrap().files,
            ["x.ts"]
        );
        assert_eq!(
            sed_in_place(&["--in-place=.orig", "s/a/b/", "c.go"])
                .unwrap()
                .files,
            ["c.go"]
        );
        assert_eq!(
            sed_in_place(&["-n", "10,20p", "src/lib.rs"]),
            None,
            "a range read"
        );
        assert_eq!(sed_in_place(&["s/a/b/"]), None, "a stream edit");
    }

    #[test]
    fn paths_classify() {
        assert!(is_source_file("crates/x/src/lib.rs"));
        assert!(!is_source_file("dist/app.min.js"));
        assert!(!is_source_file("notes.md"));
        assert!(outside_project("~/.cargo/registry/src/foo/lib.rs"));
        assert!(outside_project("node_modules/x/index.js"));
        assert!(outside_project("target/debug/build/out.rs"));
        assert!(!outside_project("crates/pa-core/src/lib.rs"));
        assert!(is_bundle("assets/chunk-8ejb9ca1.js"));
        assert!(is_bundle("extracted/cli.js"));
        assert!(!is_bundle("src/app.js"));
        assert!(unbounded_bundle_grep(
            "rg",
            &["-n", "sendQueued", "assets/chunk-1.js"]
        ));
        assert!(!unbounded_bundle_grep(
            "rg",
            &["-o", ".{0,80}sendQueued.{0,80}", "assets/chunk-1.js"]
        ));
        assert!(is_scratch("/tmp/x.rs"));
    }
}
