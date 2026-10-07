//! Reading one stage: which word it runs behind the prefix the shell and its
//! wrappers consume, and the wrapper forms that run a shell or a command line
//! of their own (`sudo -s`, `env -S '...'`).

use super::region::{is_py_digit, Word};

/// Commands whose output is remote code when the far end of a pipe is a URL.
pub(super) const DOWNLOAD_COMMANDS: [&str; 2] = ["curl", "wget"];

/// The four shells this guard covers (other shells and interpreters are out
/// of scope for this foot-gun guard).
pub(super) const SHELL_INTERPRETERS: [&str; 4] = ["sh", "bash", "zsh", "dash"];

/// Interpreters plus the shell's own run-a-string builtins.
const RUNNERS: [&str; 7] = ["sh", "bash", "zsh", "dash", "eval", "source", "."];

/// Wrapper commands whose named word is the command word.
const WRAPPER_COMMANDS: [&str; 13] = [
    "env", "time", "nice", "nohup", "command", "builtin", "exec", "timeout", "stdbuf", "ionice",
    "xargs", "busybox", "sudo",
];

/// Reserved words that group, negate, or bracket a compound command without
/// being the command themselves.
const RESERVED_WORDS: [&str; 17] = [
    "!", "{", "}", "coproc", "if", "then", "elif", "else", "fi", "do", "done", "while", "until",
    "for", "in", "case", "esac",
];

/// The command name a word runs: its basename, as the shell resolves it.
pub(super) fn command_name(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

pub(super) fn is_download(value: &str) -> bool {
    DOWNLOAD_COMMANDS.contains(&command_name(value))
}

pub(super) fn is_runner(value: &str) -> bool {
    RUNNERS.contains(&command_name(value))
}

/// Flags that consume the following word, per wrapper (`env -a NAME` renames
/// argv\[0\] of the command env runs, so its word is that name).
fn wrapper_value_flags(name: &str) -> &'static [&'static str] {
    match name {
        "env" => &["-u", "-C", "-S", "-a"],
        "nice" => &["-n"],
        "timeout" => &["-s", "-k"],
        "stdbuf" => &["-i", "-o", "-e"],
        "ionice" => &["-c", "-n", "-p", "-P", "-u"],
        "sudo" => &["-u", "-g", "-p", "-C", "-h", "-U", "-T", "-R", "-D"],
        "xargs" => &["-I", "-E", "-L"],
        _ => &[],
    }
}

/// `NAME=` prefix words: the shell runs the rest of the stage with those
/// variables bound.
fn is_assignment(value: &str) -> bool {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
    {
        return false;
    }
    for char in chars {
        if char == '=' {
            return true;
        }
        if !(char.is_ascii_alphanumeric() || char == '_') {
            return false;
        }
    }
    false
}

/// A bare duration word `timeout` takes as its operand (`timeout 30s sh`).
fn is_duration(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    let mut index = 0;
    let digits = |index: &mut usize| {
        let start = *index;
        while chars
            .get(*index)
            .is_some_and(|char| char.is_numeric() && is_py_digit(*char))
        {
            *index += 1;
        }
        *index > start
    };
    if !digits(&mut index) {
        return false;
    }
    if chars.get(index) == Some(&'.') {
        let before = index;
        index += 1;
        if !digits(&mut index) {
            index = before;
        }
    }
    if chars
        .get(index)
        .is_some_and(|char| matches!(char, 's' | 'm' | 'h' | 'd'))
    {
        index += 1;
    }
    // Python's `$` also matches before one trailing newline.
    index == chars.len() || (index + 1 == chars.len() && chars[index] == '\n')
}

/// A bundled short-option cluster, read the way getopt reads it: the FIRST
/// value-taking character either ends the cluster (`-su root` binds root as
/// `-u`'s operand) or has the rest attached as its operand (`-uMath sh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Cluster {
    /// A sudo `-s`/`-i` before the value-taking character.
    pub shell: bool,
    /// The next word is the cluster's operand.
    pub takes_next: bool,
}

pub(super) fn cluster_flags(name: &str, value: &str) -> Cluster {
    let chars: Vec<char> = value.chars().collect();
    if chars.first() != Some(&'-') || value.starts_with("--") || chars.len() < 2 {
        return Cluster::default();
    }
    let value_chars: Vec<char> = wrapper_value_flags(name)
        .iter()
        .filter_map(|flag| {
            let mut flag = flag.chars();
            flag.next();
            let letter = flag.next()?;
            flag.next().is_none().then_some(letter)
        })
        .collect();
    let mut shell = false;
    for (position, char) in chars[1..].iter().enumerate() {
        if value_chars.contains(char) {
            return Cluster {
                shell,
                takes_next: chars.len() - 2 == position,
            };
        }
        if name == "sudo" && matches!(char, 's' | 'i') {
            shell = true;
        }
    }
    Cluster {
        shell,
        takes_next: false,
    }
}

/// The word a stage would run, with its index: the first word after the
/// prefix the shell consumes before the command runs. Assignments, reserved
/// words, wrappers, their flags and value words, and bare numbers compose in
/// any order; `command -v X` only looks X up, so that lookup ends the prefix.
pub(super) fn command_word(words: &[Word]) -> Option<usize> {
    let mut index = 0;
    while index < words.len() {
        let value = words[index].value.as_str();
        let name = command_name(value);
        if is_assignment(value) {
            index += 1;
            continue;
        }
        if RESERVED_WORDS.contains(&value) && !words[index].quoted {
            index += 1;
            continue;
        }
        if !WRAPPER_COMMANDS.contains(&name) {
            break;
        }
        if name == "command"
            && words
                .get(index + 1)
                .is_some_and(|next| matches!(next.value.as_str(), "-v" | "-V"))
        {
            break;
        }
        index += 1;
        let value_flags = wrapper_value_flags(name);
        while index < words.len() && words[index].value.starts_with('-') {
            let flag = words[index].value.as_str();
            if value_flags.contains(&flag) || cluster_flags(name, flag).takes_next {
                index += 2;
            } else {
                index += 1;
            }
        }
        if let Some(word) = words.get(index) {
            let value = word.value.as_str();
            if (!value.is_empty() && value.chars().all(is_py_digit))
                || (name == "timeout" && is_duration(value))
            {
                index += 1;
            }
        }
    }
    (index < words.len()).then_some(index)
}

/// The operand an `env -S` prefix hands the shell as a command line.
pub(super) fn env_s_operand(words: &[Word]) -> Option<Word> {
    for (index, word) in words.iter().enumerate() {
        if command_name(&word.value) != "env" {
            continue;
        }
        let mut cursor = index + 1;
        while cursor < words.len() {
            let value = words[cursor].value.as_str();
            // The exact and attached forms come first: an attached `-S<...>`
            // operand does not end in S by accident of its payload.
            if value == "-S" || value == "--split-string" {
                return words.get(cursor + 1).cloned();
            }
            if let Some(operand) = value
                .strip_prefix("-S")
                .filter(|operand| !operand.is_empty())
            {
                return Some(Word::quoted_literal(operand.to_string()));
            }
            if let Some(operand) = value.strip_prefix("--split-string=") {
                return Some(Word::quoted_literal(operand.to_string()));
            }
            if value.starts_with('-')
                && !value.starts_with("--")
                && value.ends_with('S')
                && !value.is_empty()
            {
                // A bundled cluster ending in `-S` (`-iS`) takes the next word.
                return words.get(cursor + 1).cloned();
            }
            if value.starts_with('-') {
                cursor += if cluster_flags("env", value).takes_next {
                    2
                } else {
                    1
                };
                continue;
            }
            break;
        }
    }
    None
}

/// Whether a wrapper-only stage starts a shell reading stdin: `sudo -s` and
/// `sudo -i` with no further command run the user's shell with the
/// pipeline's output on stdin, exactly like a bare `sh`.
pub(super) fn runs_stdin_shell(words: &[Word]) -> bool {
    let Some(index) = words
        .iter()
        .position(|word| command_name(&word.value) == "sudo")
    else {
        return false;
    };
    let value_flags = wrapper_value_flags("sudo");
    let mut cursor = index + 1;
    let mut shell_flag = false;
    while cursor < words.len() {
        let value = words[cursor].value.as_str();
        if value_flags.contains(&value) {
            cursor += 2; // the flag's operand is not a command
            continue;
        }
        let cluster = cluster_flags("sudo", value);
        if cluster.takes_next {
            shell_flag |= cluster.shell;
            cursor += 2;
        } else if value == "--shell"
            || value == "--login"
            || (value.starts_with('-')
                && !value.starts_with("--")
                && value.chars().count() > 1
                && (value.contains('s') || value.contains('i')))
        {
            shell_flag = true;
            cursor += 1;
        } else if value.starts_with('-') {
            cursor += 1;
        } else {
            return false; // a command follows: this is a normal sudo
        }
    }
    shell_flag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(values: &[&str]) -> Vec<Word> {
        values
            .iter()
            .map(|value| Word {
                value: (*value).to_string(),
                substitutions: Vec::new(),
                resolvable: true,
                quoted: false,
            })
            .collect()
    }

    #[test]
    fn the_command_word_skips_wrappers_and_their_values() {
        assert_eq!(
            command_word(&words(&[
                "FOO=1", "env", "-u", "X", "nice", "5", "curl", "URL"
            ])),
            Some(6)
        );
        assert_eq!(command_word(&words(&["timeout", "30s", "sh"])), Some(2));
        assert_eq!(command_word(&words(&["command", "-v", "sh"])), Some(0));
        assert_eq!(command_word(&words(&["sudo", "-su", "root"])), None);
    }

    #[test]
    fn clusters_read_like_getopt() {
        assert_eq!(
            cluster_flags("sudo", "-su"),
            Cluster {
                shell: true,
                takes_next: true
            }
        );
        assert_eq!(
            cluster_flags("sudo", "-uMath"),
            Cluster {
                shell: false,
                takes_next: false
            }
        );
        assert_eq!(cluster_flags("env", "--unset"), Cluster::default());
    }
}
