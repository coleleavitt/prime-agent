//! Steering reminders after a python cell, for the habits the 2026-11-15
//! corpus study found the bash guards cannot refuse:
//!
//! - a grep run through `subprocess`/`os.system` (it skips every `bash()`
//!   guard) that looks up a definition or call site in project source:
//!   point at `rlm.lsp` and `codeflow`;
//! - a fetch that hit bot protection: point at the `camoufox-browser` skill;
//! - `sed -i` on config or other non-source files, and `sed -i` over files
//!   only known at run time (`find -exec`, `xargs`): not checkpointed;
//! - a grep over minified bundles without bounded output;
//! - plain-Python writes to project source (`write_text`, `open(.., "w")`):
//!   no /rewind checkpoint, permission check or diff, unlike the edit skill;
//! - a structural grep that ran past the `grep` guard (`allow_grep=True`).
//!
//! [`refuse_text_definition_lookup`] refuses, before the cell runs, a python
//! cell that looks up a definition by text (`src.find("fn x")` then a slice,
//! `if "fn x" in line`, `re.search(r"fn \w+")`): grep by another name. The
//! 2026-11-16 corpus (100k cells) had 1,376 such cells, every one of 30
//! sampled a definition lookup, and in 31% of the turns where the user asked
//! for codeflow the agent read text instead and never called codeflow or lsp.
//!
//! Detection is pure over the cell's code, its finished `bash()` commands,
//! its output and the files it wrote; the shapes come from
//! [`pa_bash::search`], the same classifier the `grep`/`sed` guards use.
//! The feature throttles.

use std::path::Path;
use std::sync::OnceLock;

use pa_bash::search::{
    is_scratch,
    is_source_file,
    outside_project,
    sed_in_place,
    structural_grep,
    unbounded_bundle_grep,
};
use regex::Regex;

/// Where the codeflow reminders and refusals point.
const STRUCTURAL_TOOLS: &str = "Use codeflow.tree (callers, callees), codeflow.cfg (every path; level=\"mir\" for Rust) and rlm.lsp (definition, references, workspace_symbols: they give the file and line to read), and quote their file:line output. The user has asked for this repeatedly.";

/// The marker a cell carries to look up code by text on purpose.
pub(crate) const ALLOW_TEXT_SEARCH_MARKER: &str = "# pa: allow-text-search";

/// The marker a cell carries to write source with plain Python on purpose.
pub(crate) const ALLOW_WRITE_MARKER: &str = "# pa: allow-write";

/// A habit to steer away from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Steer {
    /// A structural grep run outside `bash()`.
    Codeflow,
    /// A fetch that hit bot protection.
    Camoufox,
    /// `sed -i` on a non-source file.
    SedConfig,
    /// `sed -i` over files only known at run time.
    SedMulti,
    /// A grep over minified bundles without bounded output.
    Bundle,
    /// Plain-Python writes to project source.
    PythonWrite,
}

/// One reminder: the habit and its text (some name the files involved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reminder {
    pub(crate) habit: Steer,
    pub(crate) text: String,
}

fn reminder(habit: Steer, text: impl Into<String>) -> Reminder {
    Reminder {
        habit,
        text: text.into(),
    }
}

/// What one finished python cell did.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cell<'a> {
    pub(crate) code: &'a str,
    /// Its finished `bash()` commands.
    pub(crate) commands: &'a [String],
    pub(crate) output: &'a str,
    /// The files it opened for writing.
    pub(crate) writes: &'a [String],
    /// The session's working directory (the project).
    pub(crate) project: &'a Path,
}

/// Shell words of `segment`, with single and double quotes and backslash
/// escapes resolved.
fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    word.push(inner);
                }
            }
            '"' => {
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                if !matches!(escaped, '"' | '\\' | '$' | '`') {
                                    word.push('\\');
                                }
                                word.push(escaped);
                            }
                        }
                        _ => word.push(inner),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// The simple commands of a shell line: each segment between `|`, `;`,
/// `&&`, `||` and newlines (outside quotes), with whether a pipe feeds it.
fn segments(command: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut piped = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        // `2>&1`, `&>file`, `>&2`: a redirection, not a separator.
        if quote.is_none() && c == '&' && (current.ends_with('>') || chars.peek() == Some(&'>')) {
            current.push(c);
            continue;
        }
        match (quote, c) {
            (Some(open), c) if c == open => {
                quote = None;
                current.push(c);
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            (None, '|' | ';' | '&' | '\n') => {
                let doubled = chars.peek() == Some(&c);
                if doubled {
                    chars.next();
                }
                if !current.trim().is_empty() {
                    out.push((std::mem::take(&mut current), piped));
                }
                current.clear();
                piped = c == '|' && !doubled;
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push((current, piped));
    }
    out
}

/// A command's program (basename, without leading assignments and the
/// usual wrappers) and its argument words.
fn program_and_args(words: &[String]) -> Option<(String, Vec<String>)> {
    let mut rest = words
        .iter()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'))
        .peekable();
    loop {
        let word = rest.peek()?;
        let base = word.rsplit('/').next().unwrap_or(word);
        if matches!(base, "sudo" | "env" | "nice" | "time" | "command" | "exec") {
            rest.next();
        } else if base == "timeout" {
            rest.next();
            rest.next();
        } else {
            break;
        }
    }
    let program = rest.next()?;
    let base = program
        .rsplit('/')
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let mut args: Vec<String> = rest.cloned().collect();
    if base == "git" && args.first().map(String::as_str) == Some("grep") {
        args.remove(0);
        return Some(("git-grep".to_string(), args));
    }
    Some((base, args))
}

fn subprocess_call() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?s)(?:subprocess\.(?:run|call|check_call|check_output|Popen|getoutput|getstatusoutput)|os\.(?:system|popen))\(\s*(\[.*?\]|[rfb]{0,2}"(?:[^"\\]|\\.)*"|[rfb]{0,2}'(?:[^'\\]|\\.)*')"#,
        )
        .expect("the subprocess pattern is a valid regex")
    })
}

fn string_literal() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#""((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'"#)
            .expect("the literal pattern is a valid regex")
    })
}

/// The shell commands a cell's code runs through `subprocess` or
/// `os.system`, as (program, args) pairs.
fn subprocess_commands(code: &str) -> Vec<(String, Vec<String>, bool)> {
    let mut out = Vec::new();
    for captures in subprocess_call().captures_iter(code) {
        let argument = &captures[1];
        if argument.starts_with('[') {
            let words: Vec<String> = string_literal()
                .captures_iter(argument)
                .filter_map(|literal| literal.get(1).or_else(|| literal.get(2)))
                .map(|literal| literal.as_str().to_string())
                .collect();
            if let Some((program, args)) = program_and_args(&words) {
                out.push((program, args, false));
            }
        } else {
            let prefix_len = argument.find(['"', '\'']).unwrap_or(0);
            let raw = argument[..prefix_len].contains(['r', 'R']);
            let text = &argument[prefix_len..];
            let inner = &text[1..text.len().saturating_sub(1)];
            // What Python makes of the literal: `\\` is one backslash.
            let inner = if raw {
                inner.to_string()
            } else {
                inner.replace("\\\\", "\\")
            };
            for (segment, piped) in segments(&inner) {
                if let Some((program, args)) = program_and_args(&shell_words(&segment)) {
                    out.push((program, args, piped));
                }
            }
        }
    }
    out
}

/// The simple commands of the cell's finished `bash()` commands.
fn bash_commands(commands: &[String]) -> Vec<(String, Vec<String>, bool)> {
    commands
        .iter()
        .flat_map(|command| segments(command))
        .filter_map(|(segment, piped)| {
            program_and_args(&shell_words(&segment)).map(|(program, args)| (program, args, piped))
        })
        .collect()
}

fn http_client() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"\b(?:curl|wget|requests\.(?:get|post)|httpx\.|urllib\.request|aiohttp|web_fetch)\b",
        )
        .expect("the http client pattern is a valid regex")
    })
}

fn blocked() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:403\s+forbidden|429\s+too\s+many|status(?:_code)?\W{0,3}(?:403|429)|http/\S+\s+(?:403|429))\b|captcha|cf-chl|just a moment\.\.\.|attention required|blocked by network security|access denied",
        )
        .expect("the blocked pattern is a valid regex")
    })
}

/// Whether the cell's code uses the edit skill.
fn uses_edit_skill(code: &str) -> bool {
    code.contains("edit.run(")
        || code.contains("edit.write(")
        || code.contains("await edit(")
        || code.contains("edit.notebook_edit(")
}

/// The first structural grep among `commands` (the pattern it looks up).
fn first_structural_grep(commands: &[(String, Vec<String>, bool)]) -> Option<String> {
    commands.iter().find_map(|(program, args, piped)| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        structural_grep(program, &args, *piped).map(|found| found.pattern)
    })
}

/// Project source files among `writes`.
fn source_writes<'a>(writes: &'a [String], project: &Path) -> Vec<&'a str> {
    writes
        .iter()
        .map(String::as_str)
        .filter(|path| {
            is_source_file(path)
                && !is_scratch(path)
                && !outside_project(path)
                && (!Path::new(path).is_absolute() || Path::new(path).starts_with(project))
        })
        .collect()
}

/// The `sed -i` habits among `commands`: in-place edits of non-source
/// files, and in-place edits over files only known at run time.
fn sed_habits<'a>(
    commands: impl Iterator<Item = &'a (String, Vec<String>, bool)>,
) -> Vec<Reminder> {
    let mut out = Vec::new();
    let mut config_files: Vec<String> = Vec::new();
    let mut multi = false;
    for (program, args, _) in commands {
        let after_runner = if matches!(program.as_str(), "xargs" | "find" | "parallel") {
            args.iter()
                .position(|word| word == "sed")
                .map(|at| &args[at + 1..])
        } else if matches!(program.as_str(), "sed" | "gsed") {
            Some(&args[..])
        } else {
            None
        };
        let Some(sed_args) = after_runner else {
            continue;
        };
        let sed_args: Vec<&str> = sed_args.iter().map(String::as_str).collect();
        let Some(edit) = sed_in_place(&sed_args) else {
            continue;
        };
        let literal_files: Vec<&String> = edit
            .files
            .iter()
            .filter(|file| !file.contains('{') && *file != ";" && *file != "+")
            .collect();
        if !matches!(program.as_str(), "sed" | "gsed") || literal_files.is_empty() {
            multi = true;
        } else {
            config_files.extend(
                literal_files
                    .into_iter()
                    .filter(|file| {
                        !is_source_file(file) && !is_scratch(file) && !outside_project(file)
                    })
                    .cloned(),
            );
        }
    }
    if !config_files.is_empty() {
        out.push(reminder(
            Steer::SedConfig,
            format!(
                "Reminder: sed -i rewrote {} without a /rewind checkpoint, a permission check or a diff. Prefer the edit skill (edit.run / edit.write) for files you keep.",
                config_files.join(", ")
            ),
        ));
    }
    if multi {
        out.push(reminder(
            Steer::SedMulti,
            "Reminder: this cell ran sed -i over files chosen at run time (find -exec / xargs), which nothing checkpoints. Next time list the files and preview the change first (sed -n 's/old/new/p' FILE, or rg -n PATTERN), then apply it with edit.run per file.",
        ));
    }

    out
}

/// The reminders one cell earns, in a fixed order.
pub(crate) fn detect(cell: &Cell<'_>) -> Vec<Reminder> {
    let mut out = Vec::new();
    let subprocesses = subprocess_commands(cell.code);
    let ran = bash_commands(cell.commands);

    if let Some(pattern) = first_structural_grep(&subprocesses) {
        out.push(reminder(
            Steer::Codeflow,
            format!(
                "Reminder: this cell grepped project source for the code structure `{pattern}` through subprocess, which skips the bash() guards. {STRUCTURAL_TOOLS}"
            ),
        ));
    } else if let Some(pattern) = first_structural_grep(&ran) {
        out.push(reminder(
            Steer::Codeflow,
            format!(
                "Reminder: this cell ran the structural grep `{pattern}` past the grep guard (allow_grep=True is for strings, log text and config keys). {STRUCTURAL_TOOLS}"
            ),
        ));
    }

    let fetched = http_client().is_match(cell.code)
        || cell
            .commands
            .iter()
            .any(|command| http_client().is_match(command));
    if fetched && blocked().is_match(cell.output) {
        out.push(reminder(
            Steer::Camoufox,
            "Reminder: this request looks blocked (403/429, captcha or a bot challenge). Switch to the camoufox-browser skill for this site and don't retry it with curl, requests or web_fetch. The user has asked for this repeatedly.",
        ));
    }

    out.extend(sed_habits(ran.iter().chain(&subprocesses)));

    let bundle = ran.iter().chain(&subprocesses).any(|(program, args, _)| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        unbounded_bundle_grep(program, &args)
    });
    if bundle {
        out.push(reminder(
            Steer::Bundle,
            "Reminder: grep over a minified bundle prints whole lines that can run to megabytes. Bound it: rg -o '.{0,80}PATTERN.{0,80}' FILE, rg -c, or -M 300; for structure, beautify with jsbeautify or use codeflow.tree on the bundle.",
        ));
    }

    if !cell.code.contains(ALLOW_WRITE_MARKER) && !uses_edit_skill(cell.code) {
        let files = source_writes(cell.writes, cell.project);
        if !files.is_empty() {
            let shown: Vec<&str> = files
                .iter()
                .map(|path| {
                    Path::new(path)
                        .strip_prefix(cell.project)
                        .ok()
                        .and_then(Path::to_str)
                        .unwrap_or(path)
                })
                .take(5)
                .collect();
            out.push(reminder(
                Steer::PythonWrite,
                format!(
                    "Reminder: this cell wrote project source with plain Python ({}). Those writes get no /rewind checkpoint, permission check, stale-read check or diff. Use the edit skill: await edit.run(path, old_str, new_str) or await edit.write(path, text). Mark a deliberate plain write with `{ALLOW_WRITE_MARKER}`.",
                    shown.join(", ")
                ),
            ));
        }
    }
    out
}

/// `PRIME_AGENT_SOURCE_WRITES=edit-only` refuses cells that write project
/// source with plain Python before they run.
pub(crate) const SOURCE_WRITES_ENV: &str = "PRIME_AGENT_SOURCE_WRITES";

fn plain_write() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"\.write_(?:text|bytes)\(|\bopen\([^)]*,\s*(?:mode\s*=\s*)?f?["'][rb+]*[wax]"#)
            .expect("the plain write pattern is a valid regex")
    })
}

/// The refusal of a cell whose code plainly writes a source file, when the
/// opt-in is set (`PRIME_AGENT_SOURCE_WRITES=edit-only`).
pub(crate) fn refuse_plain_source_write(code: &str, edit_only: bool) -> Option<String> {
    if !edit_only || code.contains(ALLOW_WRITE_MARKER) || !plain_write().is_match(code) {
        return None;
    }
    let source = string_literal()
        .captures_iter(code)
        .filter_map(|literal| literal.get(1).or_else(|| literal.get(2)))
        .map(|literal| literal.as_str())
        .find(|text| is_source_file(text) && !is_scratch(text) && !outside_project(text))?;
    Some(format!(
        "Refused: this cell writes the source file {source} with plain Python ({SOURCE_WRITES_ENV}=edit-only is set). Use the edit skill, which checkpoints for /rewind and shows a diff: await edit.run(path, old_str, new_str) or await edit.write(path, text). Add `{ALLOW_WRITE_MARKER}` to the cell for a deliberate plain write."
    ))
}

/// A python lookup of a definition by text: `.find`/`.index`/`.rfind` of a
/// definition literal (`"fn x"`, `"pub struct X"`, `"def x"`, `"class X"`), a
/// membership test against one (`if "fn x" in line`), or a regex search whose
/// pattern names a definition keyword (`re.search(r"fn \w+", ..)`).
fn text_definition_lookup() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        let keyword = r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|def|class|struct|enum|impl|trait)\s";
        Regex::new(&format!(
            r#"\.(?:find|index|rfind)\(\s*[rbf]{{0,2}}["']\s*{keyword}|\bif\s+[rbf]{{0,2}}["']\s*{keyword}[^"'\n]*["']\s+in\s+\w+|\bre\.(?:search|findall|finditer|match)\(\s*[rbf]{{0,2}}["'](?:[^"'\n]*[^\w"'\n])?\^?(?:\\b)?(?:fn|def|class|struct|enum|impl|trait)(?:\s|\\s)"#
        ))
        .expect("the text definition lookup pattern is a valid regex")
    })
}

/// The refusal of a python cell that looks up a definition by text, before
/// it runs; [`ALLOW_TEXT_SEARCH_MARKER`] in the cell lets it through.
pub(crate) fn refuse_text_definition_lookup(code: &str) -> Option<String> {
    if code.contains(ALLOW_TEXT_SEARCH_MARKER) {
        return None;
    }
    let found = text_definition_lookup().find(code)?;
    let line_start = code[..found.start()].rfind('\n').map_or(0, |at| at + 1);
    let line_end = code[found.end()..]
        .find('\n')
        .map_or(code.len(), |at| found.end() + at);
    let line: String = code[line_start..line_end]
        .trim()
        .chars()
        .take(120)
        .collect();
    Some(format!(
        "Refused: this cell looks up code structure by text (`{line}`), which is grep by another name. {STRUCTURAL_TOOLS} Then read the lines they report. Add `{ALLOW_TEXT_SEARCH_MARKER}` to the cell for a deliberate text search."
    ))
}

/// Whether the `count`-th match of a habit in a session reminds: the first,
/// then every fifth.
pub(crate) fn due(count: u32) -> bool {
    count == 1 || count % 5 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell<'a>(
        code: &'a str,
        commands: &'a [String],
        output: &'a str,
        writes: &'a [String],
    ) -> Cell<'a> {
        Cell {
            code,
            commands,
            output,
            writes,
            project: Path::new("/work/repo"),
        }
    }

    fn habits(found: &[Reminder]) -> Vec<Steer> {
        found.iter().map(|reminder| reminder.habit).collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|text| (*text).to_string()).collect()
    }

    #[test]
    fn subprocess_structural_greps_steer_to_the_structural_tools() {
        for code in [
            "subprocess.run(['rg', '-n', 'fn send_now', 'crates/'])",
            "out = subprocess.check_output(\"rg -n 'isolate_hook\\\\(' -t rust\", shell=True)",
            "os.system(\"grep -rn 'class Session' src/\")",
            "subprocess.run(['rg', '-l', 'fn send_now', 'crates/'])",
        ] {
            assert_eq!(
                habits(&detect(&cell(code, &[], "", &[]))),
                [Steer::Codeflow],
                "{code}"
            );
        }
        for code in [
            "subprocess.run(['rg', '-n', 'TODO', 'crates/'])",
            "subprocess.run(['rg', '-l', 'isolate_hook\\(', 'crates/'])",
            "print(open('a.rs').read())",
        ] {
            assert!(detect(&cell(code, &[], "", &[])).is_empty(), "{code}");
        }
    }

    #[test]
    fn a_structural_grep_past_the_guard_steers_to_the_structural_tools() {
        let allowed = strings(&["rg -n 'fn send_now' crates/"]);
        assert_eq!(
            habits(&detect(&cell(
                "await bash(cmd, allow_grep=True)",
                &allowed,
                "",
                &[]
            ))),
            [Steer::Codeflow]
        );
        let plain = strings(&["rg -n 'TODO' crates/"]);
        assert!(detect(&cell("await bash(cmd, allow_grep=True)", &plain, "", &[])).is_empty());
    }

    #[test]
    fn text_definition_lookups_are_refused_before_the_cell_runs() {
        for code in [
            "i = src.index('fn classify'); print(src[i-1500:i+900])",
            "k = sg.find(\"pub async fn run_all_checks\")",
            "s = t.rfind('pub(crate) struct Cell')",
            "i = t.index('class OutsideTheWorkspace')",
            "i = next(n for n, l in enumerate(lines) if 'fn apply(&self' in l)",
            "for l in lines:\n    if 'pub trait ProviderRequestHooks' in l: print(l)",
            "hits = [l for l in lines if re.search(r'fn apply\\b', l)]",
            "re.findall(r'^(mod |enum Commands|struct Args)', text)",
            "re.search(r'\\bdef\\s+run', src)",
        ] {
            assert!(refuse_text_definition_lookup(code).is_some(), "{code}");
        }
        for code in [
            "i = src.find('    let disabled_body_hash = body_sha256(&body);')",
            "assert src.count(old) == 1",
            "[l for l in lines if 'ERROR' in l]",
            "re.findall(r'class=\"([^\"]+)\"', html)",
            "print(text.splitlines()[10:40])",
            "i = src.index('fn classify')  # pa: allow-text-search",
        ] {
            assert!(refuse_text_definition_lookup(code).is_none(), "{code}");
        }
    }

    #[test]
    fn a_blocked_fetch_steers_to_camoufox() {
        let code = "r = requests.get(url); print(r.status_code, r.text[:200])";
        assert_eq!(
            habits(&detect(&cell(
                code,
                &[],
                "403 <title>Just a moment...</title>",
                &[]
            ))),
            [Steer::Camoufox]
        );
        let curl = strings(&["curl -sI https://example.com"]);
        assert_eq!(
            habits(&detect(&cell(
                "",
                &curl,
                "HTTP/2 429 \nretry-after: 30",
                &[]
            ))),
            [Steer::Camoufox]
        );
        assert!(detect(&cell(code, &[], "200 OK", &[])).is_empty());
    }

    #[test]
    fn sed_and_bundle_habits_warn() {
        let config = strings(&["sed -i 's/a/b/' config.toml"]);
        let found = detect(&cell("", &config, "", &[]));
        assert_eq!(habits(&found), [Steer::SedConfig]);
        assert!(found[0].text.contains("config.toml"));
        let multi = strings(&["find . -name '*.rs' | xargs sed -i 's/a/b/'"]);
        assert_eq!(
            habits(&detect(&cell("", &multi, "", &[]))),
            [Steer::SedMulti]
        );
        let scratch = strings(&["sed -i 's/a/b/' /tmp/x.txt", "sed -n '1,5p' src/lib.rs"]);
        assert!(detect(&cell("", &scratch, "", &[])).is_empty());
        let bundle = strings(&["rg -n sendQueued assets/chunk-8ejb9ca1.js"]);
        assert_eq!(
            habits(&detect(&cell("", &bundle, "", &[]))),
            [Steer::Bundle]
        );
        let bounded = strings(&["rg -o '.{0,80}sendQueued.{0,80}' assets/chunk-8ejb9ca1.js"]);
        assert!(detect(&cell("", &bounded, "", &[])).is_empty());
    }

    #[test]
    fn plain_python_source_writes_warn_unless_marked_or_through_edit() {
        let writes = strings(&["/work/repo/src/lib.rs", "/work/repo/notes.md", "/tmp/x.rs"]);
        let code = "Path('src/lib.rs').write_text(new)";
        let found = detect(&cell(code, &[], "", &writes));
        assert_eq!(habits(&found), [Steer::PythonWrite]);
        assert!(found[0].text.contains("(src/lib.rs)"));
        let marked = format!("{ALLOW_WRITE_MARKER}\n{code}");
        assert!(detect(&cell(&marked, &[], "", &writes)).is_empty());
        assert!(
            detect(&cell(
                "await edit.run('src/lib.rs', a, b)",
                &[],
                "",
                &writes
            ))
            .is_empty()
        );
        let outside = strings(&["/elsewhere/x.rs", "/work/repo/target/debug/build/out.rs"]);
        assert!(detect(&cell(code, &[], "", &outside)).is_empty());
    }

    #[test]
    fn edit_only_refuses_plain_source_writes_before_they_run() {
        let code = "Path('crates/x/src/lib.rs').write_text(text)";
        assert!(
            refuse_plain_source_write(code, false).is_none(),
            "opt-in only"
        );
        let refused = refuse_plain_source_write(code, true).unwrap();
        assert!(refused.contains("crates/x/src/lib.rs") && refused.contains("edit.run"));
        assert!(refuse_plain_source_write("open('src/main.py', 'w').write(x)", true).is_some());
        assert!(refuse_plain_source_write("open('src/main.py').read()", true).is_none());
        assert!(refuse_plain_source_write("open('notes.md', 'w').write(x)", true).is_none());
        let marked = format!("{ALLOW_WRITE_MARKER}\n{code}");
        assert!(refuse_plain_source_write(&marked, true).is_none());
    }

    #[test]
    fn shell_parsing_handles_quotes_pipes_and_wrappers() {
        assert_eq!(
            shell_words("rg -n 'fn x' \"a b\" c\\ d"),
            ["rg", "-n", "fn x", "a b", "c d"]
        );
        let parts = segments("cargo test 2>&1 | grep 'a|b' && sed -i 's/;/,/' x.toml; echo done");
        let firsts: Vec<(String, bool)> = parts
            .iter()
            .map(|(segment, piped)| (shell_words(segment)[0].clone(), *piped))
            .collect();
        assert_eq!(
            firsts,
            [
                ("cargo".to_string(), false),
                ("grep".to_string(), true),
                ("sed".to_string(), false),
                ("echo".to_string(), false)
            ]
        );
        assert_eq!(
            program_and_args(&strings(&["FOO=1", "timeout", "5", "/usr/bin/rg", "x"])),
            Some(("rg".to_string(), strings(&["x"])))
        );
    }

    #[test]
    fn reminders_fire_first_then_every_fifth() {
        let fired: Vec<u32> = (1..=12).filter(|count| due(*count)).collect();
        assert_eq!(fired, [1, 5, 10]);
    }
}
