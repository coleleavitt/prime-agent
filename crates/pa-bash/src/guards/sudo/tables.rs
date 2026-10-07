//! The command taxonomy the sudo walk reads: wrappers and their value-taking
//! options, launchers that run their operands, lookups, runners, keywords.
//!
//! Value-taking options per wrapper come from each tool's usage synopsis. Both
//! a missing value option and a wrongly value-taking boolean stop the walk
//! (it then reads the operand as the command, or the command as an operand),
//! so the boolean options are listed beside their tool for review:
//!
//! - env: -i/--ignore-environment, -0/--null, -v/--debug boolean; -u, -C, -S,
//!   the GNU -a/--argv0 (9.5+) and --env0-from (9.12+) take a value, as does
//!   BSD -P ALTPATH. --block/--default/--ignore-signal are optional-argument
//!   (written --opt=SIG), so they must not eat the next word.
//! - timeout: -s/--signal, -k/--kill-after take a value; --preserve-status,
//!   --foreground, -v/--verbose boolean.
//! - stdbuf: -i, -o, -e take a value; no boolean options.
//! - ionice: -c/--class, -n/--classdata, -p/--pid, -P/--pgid, -u/--uid take a
//!   value; -t/--ignore boolean.
//! - nice: -n/--adjustment takes a value; -h, -V are help/version.
//! - exec: -a NAME takes a value; -l and -c boolean.
//! - strace: the short -a -b -e -E -I -o -O -p -P -s -S -u -U -X and the long
//!   forms with a required argument take a value; -c -C -D -f -i -k -n -q -t
//!   -T -v -V -w -x -y -z boolean, including -DDD.
//! - ltrace: -A -a -d -D -e -F -l -n -o -p -s -u -w -x and the longs with a
//!   required argument take a value; -c -C -f -i -L -q -S -T -r -t boolean
//!   (`-d` is kept because the walk then treats its operand as a value, fail
//!   closed, and ltrace rejects it).
//! - watch: -n/--interval, -q/--equexit (procps 4.0+), -s/--shotsdir (4.0.6+)
//!   take a value; -d, -b, -e, -g, -p, -t, -w, -c, -x boolean.
//! - faketime [options] timestamp program: -p PID and --date-prog PROG take a
//!   value; -m and -f boolean; the timestamp is positional.
//! - chroot NEWROOT [COMMAND]: --userspec and --groups take a value;
//!   --skip-chdir boolean; NEWROOT is positional.
//! - systemd-run: the options listed below take a value; --user, --system,
//!   --scope, --pty, -t, --pipe, -P, -q, --no-block, --collect,
//!   --remain-after-exit, --same-dir, --wait, --shell, --no-ask-password
//!   boolean. systemd-run has no --drop-in, --kill-who or --wait-timeout: an
//!   entry for an option the tool does not have swallows the command word.
//! - xargs: -I, -n, -a, -d, -E, -L, -P, -s, -J take a value; -0, -p, -r, -t,
//!   -x boolean; -e and -i are optional-argument forms, so the next word stays
//!   the command (fail closed).
//! - GNU parallel (the `GetOptions` specs in `src/parallel`): the listed options
//!   take a value; the optional-argument -i/--replace, -l/--max-lines and
//!   -e/--eof are absent, so the next word stays judged.

/// The names that escalate.
pub(super) const SUDO_COMMAND_WORDS: [&str; 2] = ["sudo", "doas"];

/// Programs that run the command after their own options.
pub(super) const WRAPPERS: &[&str] = &[
    "env",
    "nice",
    "nohup",
    "stdbuf",
    "timeout",
    "setsid",
    "ionice",
    "builtin",
    "exec",
    "busybox",
    "strace",
    "ltrace",
    "watch",
    "faketime",
    "systemd-run",
    "chroot",
];

/// These report on a program instead of running it, so a later sudo/doas is
/// a name being looked up, not a command being escalated.
pub(super) const LOOKUP_COMMANDS: &[&str] = &["type", "which", "whereis"];

pub(super) const SHELL_RUNNERS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// Shells plus the builtins that run text as shell code.
pub(super) fn is_payload_runner(name: &str) -> bool {
    SHELL_RUNNERS.contains(&name) || matches!(name, "eval" | "source" | ".")
}

/// Compound-command words are syntax, not programs: skipping them lets the
/// body's command word (sudo after `do`/`then`/`else`) reach command position.
pub(super) const KEYWORDS: &[&str] = &[
    "!", "time", "if", "then", "elif", "else", "fi", "do", "done", "while", "until", "for", "in",
    "case", "esac", "coproc",
];

/// How deep payloads (`sh -c`, `eval`, substitutions, aliases) are followed.
pub(super) const MAX_PAYLOAD_DEPTH: usize = 6;

/// A wrapper's value-taking options (long and short), the letters of its
/// value-taking short flags (a bundle such as `env -vu NAME` still consumes
/// an operand), and how many positional operands precede the command.
pub(super) struct WrapperOptions {
    pub options: &'static [&'static str],
    pub letters: &'static str,
    pub leading_operands: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "a data table: one arm per wrapper and its option lists"
)]
pub(super) fn wrapper_options(wrapper: &str) -> WrapperOptions {
    let (options, letters, leading_operands): (&'static [&'static str], &'static str, usize) =
        match wrapper {
            "env" => (
                &[
                    "-u",
                    "--unset",
                    "-C",
                    "--chdir",
                    "-S",
                    "--split-string",
                    "-a",
                    "--argv0",
                    "-P",
                    "--env0-from",
                ],
                "uCSaP",
                0,
            ),
            "timeout" => (&["-s", "--signal", "-k", "--kill-after"], "sk", 0),
            "stdbuf" => (
                &["-i", "--input", "-o", "--output", "-e", "--error"],
                "ioe",
                0,
            ),
            "ionice" => (
                &[
                    "-c",
                    "--class",
                    "-n",
                    "--classdata",
                    "-p",
                    "--pid",
                    "-P",
                    "--pgid",
                    "-u",
                    "--uid",
                ],
                "cnpPu",
                0,
            ),
            "nice" => (&["-n", "--adjustment"], "n", 0),
            "exec" => (&["-a", "--argv0"], "a", 0),
            "strace" => (
                &[
                    "-a",
                    "-b",
                    "-e",
                    "-E",
                    "-I",
                    "-o",
                    "-O",
                    "-p",
                    "-P",
                    "-s",
                    "-S",
                    "-u",
                    "-U",
                    "-X",
                    "--abbrev",
                    "--argv0",
                    "--attach",
                    "--color",
                    "--columns",
                    "--const-print-style",
                    "--decode-pids",
                    "--detach-on",
                    "--env",
                    "--fault",
                    "--inject",
                    "--interruptible",
                    "--kvm",
                    "--output",
                    "--raw",
                    "--read",
                    "--signals",
                    "--stack-trace-frame-limit",
                    "--status",
                    "--string-limit",
                    "--summary-columns",
                    "--summary-sort-by",
                    "--summary-syscall-overhead",
                    "--syscall-limit",
                    "--trace",
                    "--trace-fds",
                    "--trace-path",
                    "--user",
                    "--verbose",
                    "--write",
                ],
                "oepsaubIPOUXSE",
                0,
            ),
            "ltrace" => (
                &[
                    "-A",
                    "-a",
                    "-d",
                    "-D",
                    "-e",
                    "-F",
                    "-l",
                    "-n",
                    "-o",
                    "-p",
                    "-s",
                    "-u",
                    "-w",
                    "-x",
                    "--align",
                    "--config",
                    "--debug",
                    "--indent",
                    "--library",
                    "--output",
                    "--where",
                ],
                "oepsluaFAwnDx",
                0,
            ),
            "watch" => (
                &["-n", "--interval", "-q", "--equexit", "-s", "--shotsdir"],
                "nqs",
                0,
            ),
            "faketime" => (&["-p", "--date-prog"], "p", 1),
            "chroot" => (&["--userspec", "--groups"], "", 1),
            "systemd-run" => (
                &[
                    "-u",
                    "-p",
                    "-E",
                    "-C",
                    "-M",
                    "-H",
                    "--capsule",
                    "--unit",
                    "--property",
                    "--setenv",
                    "--machine",
                    "--uid",
                    "--gid",
                    "--host",
                    "--job-mode",
                    "--service-type",
                    "--working-directory",
                    "--slice",
                    "--description",
                    "--nice",
                    "--background",
                    "--expand-environment",
                    "--json",
                    "--on-active",
                    "--on-boot",
                    "--on-calendar",
                    "--on-startup",
                    "--on-unit-active",
                    "--on-unit-inactive",
                    "--output",
                    "--path-property",
                    "--root-directory",
                    "--socket-property",
                    "--timer-property",
                ],
                "upEMCH",
                0,
            ),
            _ => (&[], "", 0),
        };
    WrapperOptions {
        options,
        letters,
        leading_operands,
    }
}

/// Launchers that run their first non-flag word as a command (`xargs`,
/// `parallel`): their value-taking options and short letters.
#[expect(
    clippy::too_many_lines,
    reason = "a data table: one arm per launcher and its option lists"
)]
pub(super) fn launcher_operand_options(
    launcher: &str,
) -> Option<(&'static [&'static str], &'static str)> {
    match launcher {
        "xargs" => Some((
            &[
                "-I",
                "--replace",
                "-n",
                "--max-args",
                "-a",
                "--arg-file",
                "-d",
                "--delimiter",
                "-E",
                "--eof",
                "-L",
                "--max-lines",
                "-P",
                "--max-procs",
                "-s",
                "--max-chars",
                "-J",
                "--process-slot-var",
            ],
            "InadELPsJ",
        )),
        "parallel" => Some((
            &[
                "-j",
                "-N",
                "-n",
                "-L",
                "-S",
                "-a",
                "-I",
                "-C",
                "-d",
                "-D",
                "-E",
                "-J",
                "-P",
                "-s",
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
                "--debug",
                "--delimiter",
                "--profile",
                "--max-procs",
                "--max-chars",
                "--halt",
                "--halt-on-error",
                "--nice",
                "--env",
                "--workdir",
                "--work-dir",
                "--wd",
                "--sshdelay",
                "--sshloginfile",
                "--slf",
                "--recstart",
                "--recend",
                "--block",
                "--block-size",
                "--basefile",
                "--bf",
                "--arg-sep",
                "--arg-file-sep",
                "--header",
                "--minversion",
                "--min-version",
                "--return",
                "--trc",
                "--trim",
                "--compress-program",
                "--decompress-program",
                "--semaphorename",
                "--id",
                "--semaphoretimeout",
                "--seqreplace",
                "--slotreplace",
                "--dirnamereplace",
                "--dnr",
                "--basenamereplace",
                "--bnr",
                "--basenameextensionreplace",
                "--bner",
                "--extensionreplace",
                "--er",
                "--parens",
            ],
            "jNnLSaI",
        )),
        _ => None,
    }
}

/// `find [path...] -exec|-execdir|-ok|-okdir COMMAND`, and `fd -x/-X`: the
/// flags whose following word is the command (fd treats everything after its
/// flag as the command line, so its own value options are not consulted).
pub(super) fn exec_launcher_flags(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "find" => Some(&["-exec", "-execdir", "-ok", "-okdir"]),
        "fd" | "fdfind" => Some(&["-x", "--exec", "-X", "--exec-batch"]),
        _ => None,
    }
}

/// Shell builtins the command hash table cannot shadow: bash consults the
/// table only after reserved words, functions and builtins.
pub(super) const SHADOWPROOF_BUILTINS: &[&str] = &[
    "alias", "builtin", "command", "eval", "exec", "hash", "source", ".", "type",
];
