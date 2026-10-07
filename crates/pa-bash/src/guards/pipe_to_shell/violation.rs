//! The fail-closed reading of what each stage feeds: a download piped into a
//! runner, a download substituted into a runner's argv or script, and every
//! nested text a runner executes (substitutions, `-c` payloads, `eval`
//! arguments, `env -S` operands, here-document bodies).

use super::region::{scan_region, Region, Separator, Stage, Word};
use super::stage::{
    command_name, command_word, env_s_operand, is_download, is_runner, runs_stdin_shell,
    SHELL_INTERPRETERS,
};

/// Nesting of command substitutions the scan follows before refusing.
const MAX_SUBSTITUTION_SCAN_DEPTH: usize = 16;

/// Nesting of folded command-line strings `text_runs_download` follows.
const MAX_TEXT_DEPTH: usize = 4;

/// Why a command would run a curl/wget download through a shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Violation {
    PipedIntoShell,
    PipedIntoUnresolvable,
    SubstitutedIntoShell,
    NestedTooDeeply,
    /// An unterminated quote left the region unresolvable and the loose
    /// shape is still visible with its quotes dropped.
    LooseShape,
}

impl Violation {
    /// The phrase the refusal message carries.
    pub(super) fn phrase(self) -> &'static str {
        match self {
            Violation::PipedIntoShell => "a download piped into a shell",
            Violation::PipedIntoUnresolvable => {
                "a download piped into a command the scan cannot resolve"
            }
            Violation::SubstitutedIntoShell => "a download substituted into a shell",
            Violation::NestedTooDeeply => "substitutions nested too deeply for the scan to read",
            Violation::LooseShape => "a download piped or substituted into a shell",
        }
    }
}

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

/// Why `text[start..end]` would run a download through a shell, or `None`.
pub(super) fn violation(
    text: &[char],
    start: usize,
    end: usize,
    depth: usize,
) -> Option<Violation> {
    if depth > MAX_SUBSTITUTION_SCAN_DEPTH {
        // Fail closed, stating only what the scan knows: the region was not read.
        return Some(Violation::NestedTooDeeply);
    }
    let region = scan_region(text, start, end);
    if let Some(violation) = stage_violation(text, &region, depth) {
        return Some(violation);
    }
    for &(nested_start, nested_end) in &region.substitutions {
        if let Some(violation) = violation(text, nested_start, nested_end, depth + 1) {
            return Some(violation);
        }
    }
    if region.unterminated_quote && loose_shape(&text[start..end.min(text.len()).max(start)]) {
        return Some(Violation::LooseShape);
    }
    None
}

/// [`violation`] of a whole folded string at depth `depth`.
pub(super) fn text_violation(text: &str, depth: usize) -> Option<Violation> {
    let text = chars(text);
    violation(&text, 0, text.len(), depth)
}

/// Python's `str.isspace`.
fn is_py_space(char: char) -> bool {
    char.is_whitespace() || matches!(char, '\u{1c}'..='\u{1f}')
}

/// Python's `\w` for the loose pattern's word boundaries.
fn is_word_char(char: char) -> bool {
    char.is_alphanumeric() || char == '_'
}

/// The loose shape of a region an unterminated quote left unresolvable, with
/// its quote and escape characters dropped: a download word, a pipe, and an
/// interpreter word after it, on one line without a `;`
/// (`\b(?:curl|wget)\b[^;\n]*\|[^;\n]*\b(?:sh|bash|zsh|dash)\b`).
fn loose_shape(region: &[char]) -> bool {
    let text: Vec<char> = region
        .iter()
        .copied()
        .filter(|char| !matches!(char, '\'' | '"' | '\\'))
        .collect();
    let word_at = |start: usize, name: &str| {
        let name: Vec<char> = name.chars().collect();
        let end = start + name.len();
        text.get(start..end) == Some(name.as_slice())
            && (start == 0 || !is_word_char(text[start - 1]))
            && text.get(end).is_none_or(|char| !is_word_char(*char))
    };
    for start in 0..text.len() {
        let Some(download_end) = ["curl", "wget"]
            .into_iter()
            .find(|name| word_at(start, name))
            .map(|name| start + name.len())
        else {
            continue;
        };
        let line_end = text[download_end..]
            .iter()
            .position(|char| matches!(char, ';' | '\n'))
            .map_or(text.len(), |offset| download_end + offset);
        let Some(pipe) = text[download_end..line_end]
            .iter()
            .position(|char| *char == '|')
        else {
            continue;
        };
        let after_pipe = download_end + pipe + 1;
        if (after_pipe..line_end).any(|position| {
            SHELL_INTERPRETERS
                .iter()
                .any(|name| word_at(position, name))
        }) {
            return true;
        }
    }
    false
}

/// Whether this region runs curl/wget as a command word, at any nesting.
fn region_runs_download(text: &[char], start: usize, end: usize, depth: usize) -> bool {
    if depth > MAX_SUBSTITUTION_SCAN_DEPTH {
        return true; // absurdly nested: refuse rather than risk a miss
    }
    let region = scan_region(text, start, end);
    for stage in &region.stages {
        if command_word(&stage.words).is_some_and(|index| is_download(&stage.words[index].value)) {
            return true;
        }
        // A here-document body inside this region is live text: whatever the
        // region feeds runs it.
        if stage
            .heredoc_bodies
            .iter()
            .any(|body| region_runs_download(text, body.span.0, body.span.1, depth + 1))
        {
            return true;
        }
    }
    region
        .substitutions
        .iter()
        .any(|&(nested_start, nested_end)| {
            region_runs_download(text, nested_start, nested_end, depth + 1)
        })
}

/// Whether a word's own substitutions run a download, so a stage whose
/// command word is one (`$(curl ...) | sh`) feeds the download downstream.
fn word_runs_download(text: &[char], word: &Word) -> bool {
    word.substitutions
        .iter()
        .any(|&(start, end)| region_runs_download(text, start, end, 0))
}

/// `^-[A-Za-z]*c[A-Za-z]*$`: a `-c`-style flag hands the next word to the
/// interpreter as its script.
fn is_payload_flag(value: &str) -> bool {
    let value = value.strip_suffix('\n').unwrap_or(value);
    value.strip_prefix('-').is_some_and(|letters| {
        letters.contains('c') && letters.chars().all(|char| char.is_ascii_alphabetic())
    })
}

/// Whether the words a runner was handed are a download it would run: a
/// `-c`-style flag's script, every argument of `eval` (joined too, since
/// `eval` concatenates them), or the file `source`/`.` names. Words a runner
/// does not execute stay data.
fn stage_payload_runs_download(stage: &Stage, command_index: usize) -> bool {
    let words = &stage.words;
    let name = command_name(&words[command_index].value);
    if name == "eval" {
        let args = &words[command_index + 1..];
        let joined = args
            .iter()
            .map(|word| word.value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() && text_violation(&joined, 0).is_some() {
            return true;
        }
        return args
            .iter()
            .any(|word| text_violation(&word.value, 0).is_some());
    }
    if name == "source" || name == "." {
        return words
            .get(command_index + 1)
            .is_some_and(|operand| text_violation(&operand.value, 0).is_some());
    }
    (command_index + 1..words.len().saturating_sub(1)).any(|index| {
        let value = words[index].value.as_str();
        (value == "--command" || is_payload_flag(value))
            && text_violation(&words[index + 1].value, 0).is_some()
    })
}

/// Whether a shell interpreter's argv carries a substitution that runs a
/// download of its own (`sh -c "$(curl ...)"`, `sh <<< "$(curl ...)"`).
fn stage_args_run_download(text: &[char], stage: &Stage, command_index: usize) -> bool {
    stage.words[command_index + 1..]
        .iter()
        .flat_map(|word| word.substitutions.iter())
        .chain(stage.target_substitutions.iter())
        .any(|&(start, end)| region_runs_download(text, start, end, 0))
}

/// Whether a here-document body's own text runs a download through a shell:
/// the violation scan of the text plus the command-word walk of its stages
/// (an expansion's output executed as a command).
fn heredoc_text_runs_a_download(body: &[char], depth: usize) -> bool {
    if violation(body, 0, body.len(), depth + 1).is_some() {
        return true;
    }
    scan_region(body, 0, body.len()).stages.iter().any(|stage| {
        command_word(&stage.words)
            .is_some_and(|index| word_runs_download(body, &stage.words[index]))
    })
}

/// Whether this stage's here-document bodies are a script that runs a
/// download through a shell. An unquoted body is also read as the shell
/// delivers it: the read-time pass unescapes `$`, the backtick and the
/// backslash and drops a backslash-newline, so a `\$`-hidden `$(curl ...)`
/// arrives as live text.
fn stage_heredoc_body_runs_download(text: &[char], stage: &Stage, depth: usize) -> bool {
    stage.heredoc_bodies.iter().any(|body| {
        let raw = &text[body.span.0..body.span.1];
        if heredoc_text_runs_a_download(raw, depth) {
            return true;
        }
        if body.quoted {
            return false;
        }
        let mut delivered = Vec::with_capacity(raw.len());
        let mut index = 0;
        while index < raw.len() {
            if raw[index] == '\\' && index + 1 < raw.len() {
                if raw[index + 1] == '\n' {
                    index += 2;
                    continue;
                }
                if matches!(raw[index + 1], '$' | '`' | '\\') {
                    delivered.push(raw[index + 1]);
                    index += 2;
                    continue;
                }
            }
            delivered.push(raw[index]);
            index += 1;
        }
        delivered != raw && heredoc_text_runs_a_download(&delivered, depth)
    })
}

/// Whether a command-line string runs curl/wget anywhere in it: as a stage's
/// command word, or inside a word that carries shell separators (a folded
/// `-c` payload).
fn text_runs_download(text: &str, depth: usize) -> bool {
    if depth > MAX_TEXT_DEPTH {
        return true; // fail closed: absurdly nested text
    }
    let text = chars(text);
    scan_region(&text, 0, text.len())
        .stages
        .iter()
        .any(|stage| {
            command_word(&stage.words).is_some_and(|index| is_download(&stage.words[index].value))
                || stage.words.iter().any(|word| {
                    word.value.chars().any(|char| {
                        is_py_space(char) || matches!(char, '\n' | ';' | '|' | '&' | '(' | ')')
                    }) && text_runs_download(&word.value, depth + 1)
                })
        })
}

/// Whether a redirection target of this stage is a process substitution
/// that runs a shell: `>(sh)` executes the stage's output.
fn stage_targets_run_shell(text: &[char], stage: &Stage) -> bool {
    stage.target_substitutions.iter().any(|&(start, end)| {
        scan_region(text, start, end)
            .stages
            .iter()
            .any(|inner| match command_word(&inner.words) {
                Some(index) => is_runner(&inner.words[index].value),
                None => runs_stdin_shell(&inner.words),
            })
    })
}

/// Whether the stages this one pipes into (its own continuation) run a
/// shell: a here-document body is a script only when the pipe chain it
/// feeds reaches an interpreter.
fn continuation_runs_shell(region: &Region, position: usize) -> bool {
    // The caller walks only stages ended by a pipe, so the pipeline is open
    // until its right-hand side arrives.
    let mut pipeline_open = true;
    for stage in &region.stages[position + 1..] {
        if stage.words.is_empty() {
            if stage.separator.ends_statement() && !pipeline_open {
                // A blank or separator ends the chain only once the
                // pipeline's right-hand side has arrived.
                return false;
            }
            continue;
        }
        match command_word(&stage.words) {
            Some(index) if is_runner(&stage.words[index].value) => return true,
            None if runs_stdin_shell(&stage.words) => return true,
            Some(_) | None => {}
        }
        pipeline_open = stage.separator.is_pipe();
        if !stage.separator.is_pipe() && !stage.separator.is_grouping() {
            // A grouping separator continues the chain (`cat <<EOF | (sh)`).
            return false;
        }
    }
    false
}

/// Why the substitutions of an unquoted here-document body run a download
/// through a shell, whatever stage owns the body: the shell expands an
/// unquoted body at read time; a quoted delimiter leaves it inert.
fn body_substitution_violation(text: &[char], stage: &Stage, depth: usize) -> Option<Violation> {
    for body in stage.heredoc_bodies.iter().filter(|body| !body.quoted) {
        let region = scan_region(text, body.span.0, body.span.1);
        for &(start, end) in &region.substitutions {
            if let Some(violation) = violation(text, start, end, depth + 1) {
                return Some(violation);
            }
        }
    }
    None
}

/// The receiving side of a pipeline that carries a download.
fn receiver_violation(stage: &Stage, resolved: Option<usize>) -> Option<Violation> {
    let Some(command_index) = resolved else {
        if runs_stdin_shell(&stage.words) {
            return Some(Violation::PipedIntoShell);
        }
        // A stage that resolves no command word (`env -a $(sh)`,
        // `FOO=$(sh)`): the consumed substitution runs with the pipeline on
        // stdin, so the unreadable operand may be the receiver.
        return stage
            .words
            .iter()
            .any(|word| !word.resolvable)
            .then_some(Violation::PipedIntoUnresolvable);
    };
    let word = &stage.words[command_index];
    if is_runner(&word.value) {
        return Some(Violation::PipedIntoShell);
    }
    // Fail closed: an unreadable receiver, or an unreadable prefix word a
    // wrapper or assignment consumed (`env -a $(sh) cat`), cannot be cleared.
    let unreadable = !word.resolvable
        || stage.words[..command_index]
            .iter()
            .any(|prefix| !prefix.resolvable);
    unreadable.then_some(Violation::PipedIntoUnresolvable)
}

/// Why these stages run a download through a shell, or `None`.
fn stage_violation(text: &[char], region: &Region, depth: usize) -> Option<Violation> {
    let mut piped_download = false;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    // A pipe opens the pipeline until its right-hand side arrives: blank
    // lines and grouping between the two do not end it.
    let mut pipeline_open = false;
    for (position, stage) in region.stages.iter().enumerate() {
        let separator = stage.separator;
        let (Some(first), Some(last)) = (stage.words.first(), stage.words.last()) else {
            // An empty stage: a grouping character, a doubled operator, or the
            // newline of a continued pipeline. A statement separator ends the
            // chain unless a pipe is still waiting for its right-hand side.
            match separator {
                Separator::OpenParen => paren_depth += 1,
                Separator::CloseParen => paren_depth = paren_depth.saturating_sub(1),
                _ if separator.ends_statement() && !pipeline_open => piped_download = false,
                _ => {}
            }
            continue;
        };
        if first.is_bare("{") {
            // A brace group keeps the stages inside it feeding one pipeline.
            brace_depth += 1;
        }
        let resolved = command_word(&stage.words);
        if piped_download {
            if let Some(violation) = receiver_violation(stage, resolved) {
                return Some(violation);
            }
        }
        if let Some(command_index) = resolved {
            let word = &stage.words[command_index];
            if is_download(&word.value) || word_runs_download(text, word) || !word.resolvable {
                // Fail closed: an unresolvable producer could be the download.
                piped_download = true;
            } else if is_runner(&word.value)
                && (stage_args_run_download(text, stage, command_index)
                    || stage_payload_runs_download(stage, command_index)
                    || stage_heredoc_body_runs_download(text, stage, depth))
            {
                return Some(Violation::SubstitutedIntoShell);
            }
        }
        if piped_download && stage_targets_run_shell(text, stage) {
            return Some(Violation::PipedIntoShell);
        }
        if let Some(operand) = env_s_operand(&stage.words) {
            // `env -S` runs its operand as a command line.
            if let Some(violation) = text_violation(&operand.value, depth + 1) {
                return Some(violation);
            }
            if text_runs_download(&operand.value, 0) {
                piped_download = true;
            }
            let operand_text = chars(&operand.value);
            for operand_stage in &scan_region(&operand_text, 0, operand_text.len()).stages {
                // The operand names the runner, so the stage's other inputs
                // are that runner's payload (`env -S 'sh' < <(curl ...)`).
                if let Some(index) = command_word(&operand_stage.words) {
                    if is_runner(&operand_stage.words[index].value)
                        && (stage_args_run_download(text, stage, index)
                            || stage_heredoc_body_runs_download(text, stage, depth))
                    {
                        return Some(Violation::SubstitutedIntoShell);
                    }
                }
            }
        }
        let owner_is_runner = resolved.is_some_and(|index| is_runner(&stage.words[index].value));
        if !stage.heredoc_bodies.is_empty() && !owner_is_runner {
            // A non-runner holds its bodies as data, with the two reads the
            // shell forces anyway: an unquoted body's substitutions expand at
            // read time, and a body piped on into a runner is its script.
            if let Some(violation) = body_substitution_violation(text, stage, depth) {
                return Some(violation);
            }
            if separator.is_pipe()
                && continuation_runs_shell(region, position)
                && stage_heredoc_body_runs_download(text, stage, depth)
            {
                return Some(Violation::PipedIntoShell);
            }
        }
        pipeline_open = separator.is_pipe();
        if brace_depth > 0 && last.is_bare("}") {
            // Close the group before the reset check: `{ curl URL; }; sh`
            // leaves no pipeline state for the next statement.
            brace_depth -= 1;
        }
        match separator {
            Separator::OpenParen => paren_depth += 1,
            Separator::CloseParen => paren_depth = paren_depth.saturating_sub(1),
            Separator::End
            | Separator::Newline
            | Separator::Semicolon
            | Separator::Background
            | Separator::And
            | Separator::Or
            | Separator::Pipe
            | Separator::PipeBoth => {}
        }
        if !separator.is_pipe() && !separator.is_grouping() && brace_depth == 0 && paren_depth == 0
        {
            piped_download = false;
        }
    }
    None
}
