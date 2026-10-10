//! Wrappers: programs that run the rest of their argv as a command (`env`,
//! `timeout`, `nice`, `sudo`, `xargs`, ...), and how far their own options
//! reach.

use super::value::Arg;

/// How a wrapper's own argv is read.
struct Spec {
    /// Short options that take a value (`-u USER`, `-uUSER`).
    short_values: &'static str,
    /// Long options that take a value (`--user USER`, `--user=USER`).
    long_values: &'static [&'static str],
    /// Long options that take none (abbreviations resolve against both
    /// lists, the way getopt does).
    long_flags: &'static [&'static str],
    /// Positional operands before the command (`timeout DURATION`).
    leading: usize,
    /// `NAME=value` words before the command are its environment.
    assignments: bool,
}

const fn spec(
    short_values: &'static str,
    long_values: &'static [&'static str],
    long_flags: &'static [&'static str],
    leading: usize,
) -> Spec {
    Spec {
        short_values,
        long_values,
        long_flags,
        leading,
        assignments: false,
    }
}

fn spec_of(name: &str) -> Option<Spec> {
    Some(match name {
        "env" => env_spec(),
        "sudo" => sudo_spec(),
        "doas" => spec("uC", &[], &[], 0),
        "nice" => spec("n", &["--adjustment"], &["--help", "--version"], 0),
        "nohup" | "setsid" | "builtin" | "busybox" | "catchsegv" | "unbuffer" | "caffeinate"
        | "command" | "valgrind" | "perf" => spec("", &[], &[], 0),
        "exec" => spec("a", &[], &[], 0),
        "time" => spec(
            "fo",
            &["--format", "--output"],
            &["--append", "--verbose", "--portability", "--quiet"],
            0,
        ),
        "stdbuf" => spec("ioe", &["--input", "--output", "--error"], &[], 0),
        "timeout" => spec(
            "sk",
            &["--signal", "--kill-after"],
            &["--foreground", "--preserve-status", "--verbose"],
            1,
        ),
        "ionice" => spec(
            "cnpPtu",
            &["--class", "--classdata", "--pid", "--pgid", "--uid"],
            &["--ignore"],
            0,
        ),
        "chrt" => spec(
            "T",
            &["--sched-runtime", "--sched-period", "--sched-deadline"],
            &[],
            1,
        ),
        "taskset" => spec("", &[], &["--all-tasks", "--cpu-list"], 1),
        "faketime" => spec("", &["--date-prog"], &["--exclude-monotonic"], 1),
        "chroot" => spec("", &["--userspec", "--groups"], &["--skip-chdir"], 1),
        "strace" => strace_spec(),
        "ltrace" => ltrace_spec(),
        "flock" => spec("Ew", &["--conflict-exit-code", "--timeout"], &[], 1),
        "systemd-run" => systemd_run_spec(),
        "unshare" | "nsenter" => spec("tSG", &["--target", "--setuid", "--setgid", "--wd"], &[], 0),
        "sshpass" => spec("pfde", &[], &[], 0),
        _ => return None,
    })
}

fn env_spec() -> Spec {
    Spec {
        assignments: true,
        ..spec(
            "uCSaP",
            &["--unset", "--chdir", "--split-string", "--argv0"],
            &[
                "--ignore-environment",
                "--null",
                "--debug",
                "--block-signal",
                "--default-signal",
                "--ignore-signal",
                "--list-signal-handling",
                "--help",
                "--version",
            ],
            0,
        )
    }
}

fn sudo_spec() -> Spec {
    Spec {
        assignments: true,
        ..spec(
            "ugpChUTRDrt",
            &[
                "--user",
                "--group",
                "--prompt",
                "--close-from",
                "--host",
                "--other-user",
                "--command-timeout",
                "--chroot",
                "--chdir",
                "--role",
                "--type",
            ],
            &[
                "--askpass",
                "--background",
                "--bell",
                "--edit",
                "--help",
                "--set-home",
                "--login",
                "--remove-timestamp",
                "--reset-timestamp",
                "--list",
                "--non-interactive",
                "--preserve-groups",
                "--stdin",
                "--shell",
                "--validate",
                "--version",
                "--preserve-env",
            ],
            0,
        )
    }
}

fn strace_spec() -> Spec {
    spec(
        "abeEIoOpPsSuUX",
        &[
            "--output",
            "--trace",
            "--signal",
            "--user",
            "--env",
            "--string-limit",
            "--argv0",
            "--attach",
            "--trace-path",
            "--columns",
            "--decode-pids",
            "--status",
            "--summary-sort-by",
        ],
        &[],
        0,
    )
}

fn ltrace_spec() -> Spec {
    spec(
        "aAdDeFlnopsuwx",
        &[
            "--output",
            "--library",
            "--indent",
            "--align",
            "--config",
            "--debug",
            "--where",
        ],
        &[],
        0,
    )
}

fn systemd_run_spec() -> Spec {
    spec(
        "upEMCH",
        &[
            "--unit",
            "--property",
            "--setenv",
            "--machine",
            "--uid",
            "--gid",
            "--host",
            "--working-directory",
            "--slice",
            "--description",
            "--nice",
            "--job-mode",
            "--service-type",
            "--on-active",
            "--on-boot",
            "--on-calendar",
            "--on-startup",
            "--on-unit-active",
            "--on-unit-inactive",
            "--timer-property",
            "--path-property",
            "--socket-property",
            "--root-directory",
            "--capsule",
            "--output",
            "--expand-environment",
            "--background",
            "--json",
        ],
        &[],
        0,
    )
}

/// A wrapper's reading of its argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unwrapped {
    /// Fields that belong to the wrapper (its name included); the command
    /// starts after them.
    pub consumed: usize,
    /// `NAME=value` assignments it applies.
    pub assignments: Vec<(String, Arg)>,
    /// `env -C DIR`, `sudo -D DIR`: the command runs elsewhere.
    pub chdir: Option<Arg>,
    /// `env -S STRING`: the string is split into the command's argv.
    pub split: Option<Arg>,
    /// `command -v`, `sudo -v`, an ambiguous option: nothing is run.
    pub runs_nothing: bool,
    /// `sudo -s`, `sudo -i` with no command: a root shell reading stdin.
    pub shell: bool,
}

/// One `--option[=value]` of wrapper `name` at `argv[*index]` (`long` is
/// the text after `--`); moves `index` past a separate value. False when
/// getopt refuses an ambiguous abbreviation, so the wrapper runs nothing.
fn long_option(
    name: &str,
    spec: &Spec,
    argv: &[Arg],
    long: &str,
    index: &mut usize,
    out: &mut Unwrapped,
) -> bool {
    let (option, glued) = match long.split_once('=') {
        Some((option, value)) => (option, Some(value)),
        None => (long, None),
    };
    let given = format!("--{option}");
    let Some((full, takes_value)) = resolve_long(spec, &given) else {
        out.runs_nothing = true;
        return false;
    };
    let value = match (takes_value, glued) {
        (true, Some(value)) => Some(Arg::Known(value.to_string())),
        (true, None) => {
            *index += 1;
            argv.get(*index).cloned()
        }
        (false, _) => None,
    };
    note(name, full, value, out);
    if matches!(
        full,
        "--list"
            | "--validate"
            | "--version"
            | "--help"
            | "--remove-timestamp"
            | "--reset-timestamp"
    ) {
        out.runs_nothing = true;
    }
    if matches!(full, "--shell" | "--login") && name == "sudo" {
        out.shell = true;
    }
    true
}

/// Whether `name` is a wrapper, and if so where its command starts.
pub(crate) fn unwrap(name: &str, argv: &[Arg]) -> Option<Unwrapped> {
    let spec = spec_of(name)?;
    let mut out = Unwrapped {
        consumed: 1,
        assignments: Vec::new(),
        chdir: None,
        split: None,
        runs_nothing: false,
        shell: false,
    };
    let mut leading = spec.leading;
    let mut index = 1;
    while index < argv.len() {
        let Some(text) = argv[index].known() else {
            break;
        };
        if text == "--" {
            index += 1;
            break;
        }
        if let Some(long) = text.strip_prefix("--") {
            if !long_option(name, &spec, argv, long, &mut index, &mut out) {
                index += 1;
                break;
            }
            index += 1;
            continue;
        }
        if name == "nice" && text.len() > 1 && text[1..].chars().all(|ch| ch.is_ascii_digit()) {
            index += 1;
            continue;
        }
        if text.starts_with('-') && text.len() > 1 {
            let letters: Vec<char> = text[1..].chars().collect();
            for (at, letter) in letters.iter().enumerate() {
                if spec.short_values.contains(*letter) {
                    let rest: String = letters[at + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        index += 1;
                        argv.get(index).cloned()
                    } else {
                        Some(Arg::Known(rest))
                    };
                    note(name, &format!("-{letter}"), value, &mut out);
                    break;
                }
                flag(name, *letter, &mut out);
            }
            index += 1;
            continue;
        }
        if name == "env" && text == "-" {
            index += 1;
            continue;
        }
        if spec.assignments {
            if let Some((variable, value)) = text.split_once('=') {
                if is_name(variable) {
                    out.assignments
                        .push((variable.to_string(), Arg::Known(value.to_string())));
                    index += 1;
                    continue;
                }
            }
        }
        if leading > 0 {
            leading -= 1;
            index += 1;
            continue;
        }
        break;
    }
    // Unknown fields (`env "$@"`) stay in the command's argv: their words
    // decide at run time.
    out.consumed = index.min(argv.len());
    Some(out)
}

/// `given` resolved against the spec's long options (exact, or a unique
/// prefix), and whether it takes a value; `None` when ambiguous.
fn resolve_long(spec: &Spec, given: &str) -> Option<(&'static str, bool)> {
    let all = spec
        .long_values
        .iter()
        .map(|option| (*option, true))
        .chain(spec.long_flags.iter().map(|option| (*option, false)));
    let mut found: Option<(&'static str, bool)> = None;
    for (option, takes_value) in all {
        if option == given {
            return Some((option, takes_value));
        }
        if option.starts_with(given) {
            if found.is_some() {
                return None;
            }
            found = Some((option, takes_value));
        }
    }
    Some(found.unwrap_or(("--unknown", false)))
}

fn note(name: &str, option: &str, value: Option<Arg>, out: &mut Unwrapped) {
    match (name, option) {
        ("env", "-C" | "--chdir") | ("sudo", "-D" | "--chdir") => out.chdir = value,
        ("env", "-S" | "--split-string") => out.split = value,
        _ => {}
    }
}

fn flag(name: &str, letter: char, out: &mut Unwrapped) {
    match (name, letter) {
        ("command", 'v' | 'V') | ("sudo", 'v' | 'l' | 'k' | 'K' | 'V' | 'h') => {
            out.runs_nothing = true;
        }
        ("sudo" | "doas", 's' | 'i') => out.shell = true,
        _ => {}
    }
}

pub(crate) fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Shells whose `-c` string, script operand or stdin is shell code.
pub(crate) const SHELLS: [&str; 12] = [
    "sh", "bash", "zsh", "dash", "ksh", "ash", "mksh", "fish", "yash", "posh", "csh", "tcsh",
];

pub(crate) fn is_shell(name: &str) -> bool {
    SHELLS.contains(&name)
}

/// A shell, or a builtin that runs its operands as code.
pub(crate) fn is_code_runner(name: &str) -> bool {
    is_shell(name) || matches!(name, "eval" | "source" | ".")
}

/// Where a shell invocation's code comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodeArg {
    /// The field at this index is the code (`-c STRING`).
    Field(usize),
    /// Code glued to its option (fish `-cSTRING`, `--command=STRING`).
    Glued(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellCode {
    /// `-c STRING` (fish also runs `-C`/`--init-command` strings).
    Strings(Vec<CodeArg>),
    /// A script file operand: its field index.
    File(usize),
    /// No script: it reads its stdin.
    Stdin,
}

/// Read a shell's options (`argv[0]` is the shell).
pub(crate) fn shell_code(argv: &[Arg]) -> ShellCode {
    let fish = argv
        .first()
        .and_then(Arg::known)
        .is_some_and(|name| name.ends_with("fish"));
    let mut strings = Vec::new();
    let mut command = false;
    let mut index = 1;
    while index < argv.len() {
        let Some(text) = argv[index].known() else {
            break;
        };
        if text == "--" || text == "-" {
            index += 1;
            break;
        }
        if let Some(long) = text.strip_prefix("--") {
            match long.split_once('=') {
                Some(("command" | "init-command", code)) if fish => {
                    strings.push(CodeArg::Glued(code.to_string()));
                }
                None if matches!(long, "rcfile" | "init-file") => index += 1,
                None if long == "command" => command = true,
                None if fish && long == "init-command" => {
                    index += 1;
                    strings.push(CodeArg::Field(index));
                }
                Some(_) | None => {}
            }
            index += 1;
            continue;
        }
        if (text.starts_with('-') || text.starts_with('+')) && text.len() > 1 {
            let letters = &text[1..];
            if fish {
                // fish's getopt: `-c`/`-C` take the code, glued or next.
                if let Some(at) = letters.find(['c', 'C']) {
                    let glued = &letters[at + 1..];
                    if glued.is_empty() {
                        index += 1;
                        strings.push(CodeArg::Field(index));
                    } else {
                        strings.push(CodeArg::Glued(glued.to_string()));
                    }
                }
                index += 1;
                continue;
            }
            if letters.contains('c') {
                command = true;
            }
            if letters.ends_with('o') || letters.ends_with('O') {
                index += 1;
            }
            if letters.contains('s') && !command {
                return ShellCode::Stdin;
            }
            index += 1;
            continue;
        }
        break;
    }
    if command && index < argv.len() {
        strings.push(CodeArg::Field(index));
    }
    if !strings.is_empty() {
        strings.retain(|code| !matches!(code, CodeArg::Field(at) if *at >= argv.len()));
        return ShellCode::Strings(strings);
    }
    match (command, index < argv.len()) {
        (false, true) => ShellCode::File(index),
        (true, _) | (false, false) => ShellCode::Stdin,
    }
}

/// `ssh [options] HOST [COMMAND...]`: the host's index and where the remote
/// command starts.
pub(crate) fn ssh_command(argv: &[Arg]) -> Option<(usize, usize)> {
    const VALUES: &str = "bcDEeFIiJLlmOopQRSWw";
    let mut index = 1;
    while index < argv.len() {
        let text = argv[index].known()?;
        if text == "--" {
            index += 1;
            break;
        }
        if text.starts_with('-') && text.len() > 1 {
            let letters: Vec<char> = text[1..].chars().collect();
            for (at, letter) in letters.iter().enumerate() {
                if VALUES.contains(*letter) {
                    if at + 1 == letters.len() {
                        index += 1;
                    }
                    break;
                }
            }
            index += 1;
            continue;
        }
        break;
    }
    (index < argv.len()).then_some((index, index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<Arg> {
        text.split(' ')
            .map(|word| Arg::Known(word.to_string()))
            .collect()
    }

    #[test]
    fn wrappers_consume_their_options() {
        assert_eq!(
            unwrap("timeout", &args("timeout -s KILL 5 git push")).map(|u| u.consumed),
            Some(4)
        );
        assert_eq!(
            unwrap("sudo", &args("sudo -u root -E git push")).map(|u| u.consumed),
            Some(4)
        );
        assert_eq!(
            unwrap("nice", &args("nice -10 make")).map(|u| u.consumed),
            Some(2)
        );
        let env = unwrap("env", &args("env -i FOO=1 -C /tmp git status")).expect("env");
        assert_eq!(env.consumed, 5);
        assert_eq!(
            env.assignments,
            vec![("FOO".to_string(), Arg::Known("1".into()))]
        );
        assert_eq!(env.chdir, Some(Arg::Known("/tmp".into())));
        assert!(
            unwrap("command", &args("command -v sudo"))
                .expect("command")
                .runs_nothing
        );
        assert!(unwrap("git", &args("git push")).is_none());
        assert_eq!(
            unwrap("env", &args("env --uns FOO git")).map(|u| u.consumed),
            Some(3)
        );
        assert!(
            unwrap("env", &args("env --i git"))
                .expect("env")
                .runs_nothing
        );
        assert_eq!(
            unwrap("env", &args("env --ignore-signal sudo id")).map(|u| u.consumed),
            Some(2)
        );
    }

    #[test]
    fn shells_and_ssh() {
        assert_eq!(
            shell_code(&args("bash -lc x")),
            ShellCode::Strings(vec![CodeArg::Field(2)])
        );
        assert_eq!(
            shell_code(&args("bash -o pipefail -c x")),
            ShellCode::Strings(vec![CodeArg::Field(4)])
        );
        assert_eq!(
            shell_code(&args("fish -C init -cwork")),
            ShellCode::Strings(vec![CodeArg::Field(2), CodeArg::Glued("work".to_string())])
        );
        assert_eq!(shell_code(&args("bash -s")), ShellCode::Stdin);
        assert_eq!(shell_code(&args("bash -x run.sh a")), ShellCode::File(2));
        assert_eq!(shell_code(&args("sh")), ShellCode::Stdin);
        assert_eq!(
            ssh_command(&args("ssh -p 22 -o X=y host bash -s")),
            Some((5, 6))
        );
        assert_eq!(ssh_command(&args("ssh host")), Some((1, 2)));
    }
}
