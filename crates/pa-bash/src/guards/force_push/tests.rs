//! Ports of the assertions in `prime-agent-runtime/test/test_bash_forcepush_guard.py`
//! that reach into the guard's internals (scan helpers, payload scans, push
//! argument parsing, the upstream probe and its timeout).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::budget::Budget;
use super::lexing::{matching_backtick, normalize_continuations, prepare, strip_escapes};
use super::payloads::{
    env_payloads_hide_force_push, eval_payloads_hide_force_push, payload_hides_force_push,
    shell_c_payloads_hide_force_push,
};
use super::push::{find_git_push_runs, is_guarded_push, parse_push_args, PushArgs};
use super::words::scan_words;
use super::{check_counting, check_with, messages, PROBE_TIMEOUT};
use crate::context::GuardContext;
use crate::script::Script;

const FORCE_PUSH_LEAF: &str = "git push -f origin main";

fn prepared(command: &str) -> String {
    let (_, normalized, _) = prepare(command, &Budget::unlimited()).expect("unlimited budget");
    normalized.into_iter().collect()
}

fn values(command: &str) -> Vec<String> {
    scan_words(&prepared(command), &Budget::unlimited())
        .expect("unlimited budget")
        .into_iter()
        .map(|word| word.value)
        .collect()
}

fn guarded_runs(command: &str) -> Vec<PushArgs> {
    let budget = Budget::unlimited();
    let words = scan_words(&prepared(command), &budget).expect("unlimited budget");
    find_git_push_runs(&words, &budget)
        .expect("unlimited budget")
        .iter()
        .map(|run| parse_push_args(&run.tokens, run.push_index))
        .filter(is_guarded_push)
        .collect()
}

fn tokens(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_string()).collect()
}

fn json(text: &str) -> String {
    serde_json::to_string(text).expect("json string")
}

fn sh_payload_chain(depth: usize, leaf: &str) -> String {
    (0..depth).fold(leaf.to_string(), |command, _| {
        format!("sh -c {}", json(&command))
    })
}

fn alternating_payload_chain(depth: usize, first: &str, leaf: &str) -> String {
    let mut command = leaf.to_string();
    for layer in 0..depth {
        let kind = if layer % 2 == 0 {
            first
        } else if first == "sh" {
            "eval"
        } else {
            "sh"
        };
        let wrapper = if kind == "sh" { "sh -c " } else { "eval " };
        command = format!("{wrapper}{}", json(&command));
    }
    command
}

fn substitution_chain(depth: usize, fanout: usize, leaf: &str, delimiter: char) -> String {
    let mut command = leaf.to_string();
    for _ in 0..depth {
        let wrapped = (0..fanout)
            .map(|_| {
                if delimiter == '$' {
                    format!("$({command})")
                } else {
                    format!("`{command}`")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        command = format!("eval {}", json(&wrapped));
    }
    command
}

fn nested_substitutions(depth: usize, fanout: usize) -> String {
    (0..depth).fold(FORCE_PUSH_LEAF.to_string(), |command, _| {
        (0..fanout)
            .map(|_| format!("$({command})"))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// A non-repository working directory with an empty HOME and hermetic git
/// configuration.
struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn env(&self, path: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PATH".to_string(), path.to_string()),
            ("HOME".to_string(), self.path().display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
        ])
    }

    fn context(&self) -> GuardContext {
        GuardContext::new(self.path(), self.env("/usr/bin:/bin"))
    }

    fn git(&self, args: &[&str], cwd: &Path) {
        crate::test_support::run_git(cwd, self.path(), args);
    }

    /// A repo with a bare remote, main pushed, and `branch` checked out
    /// tracking its own name.
    fn repo(&self, name: &str, branch: &str) -> std::path::PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir(&repo).expect("repo dir");
        let bare = self.path().join(format!("{name}-remote.git"));
        let bare_text = bare.display().to_string();
        self.git(
            &["init", "-q", "--bare", "-b", "main", &bare_text],
            self.path(),
        );
        self.git(&["init", "-q", "-b", "main"], &repo);
        self.git(&["config", "commit.gpgsign", "false"], &repo);
        std::fs::write(repo.join("file.txt"), "one\n").expect("file");
        self.git(&["add", "."], &repo);
        self.git(&["commit", "-q", "-m", "init"], &repo);
        self.git(&["remote", "add", "origin", &bare_text], &repo);
        self.git(&["push", "-q", "-u", "origin", "main"], &repo);
        self.git(&["switch", "-q", "-c", branch], &repo);
        self.git(&["push", "-q", "-u", "origin", branch], &repo);
        repo
    }
}

fn verdict(command: &str, context: &GuardContext) -> Option<String> {
    check_with(&Script::bare(command), context, PROBE_TIMEOUT).err()
}

/// `test_detection_tables`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the three vector tables of the Python suite"
)]
fn detection_tables() {
    let matching = [
        "git push --force origin main",
        "git push -f origin main",
        "git push origin main -f",
        "git push -f origin main:main",
        "git push -f origin main:refs/heads/main",
        "git push -f origin refs/heads/main",
        "git push -f origin HEAD:main",
        "git push -f origin HEAD:heads/main",
        "git push -f origin main:heads/main",
        "git push -oo -f origin main",
        "git push -f origin :main",
        "git push -f origin main:",
        "git push -f origin @{u}",
        "git push origin +main",
        "git push origin +main:main",
        "git push origin +feature",
        "git push --force",
        "git push -f",
        "git push -f origin",
        "git push -f --all",
        "git push --force --mirror origin",
        "git push --mirror origin",
        "git push --mirror",
        "git push -fv origin main",
        "git push -f origin main --",
        "git push --force --repo=origin main",
        "git push -f --delete origin main",
        "git push --force-with-lease -f origin main",
        "/usr/bin/git push -f origin main",
        "\"git\" push -f origin main",
        "git 'push' -f origin main",
        "\\git push -f origin main",
        "sudo git push -f origin main",
        "FOO=1 git push -f origin main",
        "git -C repo push -f origin main",
        "git -c foo.bar=1 push -f origin main",
        "git --git-dir=.git push -f origin main",
        "echo $(git push -f origin main)",
        "git push -f origin \\\nmain",
        "git push 2>/dev/null -f origin main",
        "git push -f origin main 2>/dev/null",
        "(git push -f origin main)",
        "{ git push -f origin main; }",
        "git push -f origin main # ship it",
        "git push -f origin main && echo done",
        "echo git push -f origin main",
        "echo main | xargs git push -f origin",
        "git push -f origin $BRANCH",
        "git push -f origin HEAD",
        "git push -f origin ma\\\nin",
        "git push -\\\nf origin main",
        "gi\\\nt push -f origin main",
        "git push -f origin \"ma\\\nin\"",
        "$'git' push -f origin main",
        "$'\\x67it' push -f origin main",
        "$'\\u0067it' push -f origin main",
        "$\"git\" push -f origin main",
        "git $'push' -f origin main",
        "git push -$'f' origin main",
        "git push $'--force' origin main",
        "GIT push -f origin main",
        "Git.exe push -f origin main",
        "/usr/bin/GIT push -f origin main",
        "git -c alias.p='push -f origin main' p",
        "git -c alias.a=p -c alias.p='push -f origin main' a",
        "git -c alias.p='push -f origin main' -C repo p",
        "git -c alias.push='status' push -f origin main",
    ];
    let unresolvable = [
        "f=-f; git push $f origin main",
        "f='-f origin'; git push $f",
        "git push $REMOTE origin main",
        "git push origin $BRANCH",
        "BRANCH=+main; git push origin $BRANCH",
        "git push origin 'main*'",
        "git push --repo=$REMOTE main",
        "git push --force-with-lease origin $BRANCH",
        "X=-f; git push --force-with-lease origin $X",
    ];
    let non_matching = [
        "git push origin main",
        "git push",
        "git push origin",
        "git push -u origin main",
        "git push --all",
        "git push --tags",
        "git push origin --delete main",
        "git push --force-with-lease origin main",
        "git push --force-with-lease=main:expected origin main",
        "git push --force-if-includes origin main",
        "git push --force-with-lease --force-if-includes origin main",
        "git push -n origin main",
        "git push -f -n origin main",
        "git push -fn origin main",
        "git push -nf origin main",
        "git push --dry-run -f origin main",
        "git push -v -q origin main",
        "git push -of origin main",
        "git checkout --force main",
        "git config push.default matching",
        "git status",
        "echo hello",
        "npm run check",
        "echo 'git push -f origin main'",
        "echo \"git push -f origin main\"",
        "# git push -f origin main",
        "git --exec-path push -f origin main",
        "git -c alias.s=status s",
        "git -c alias.co=checkout co",
        "git -c alias.push='status' push --dry-run -f origin main",
        "env -C . echo hi",
        "env -S 'git status'",
        "printf $'%s\\n' hi",
        "echo $\"hello\"",
        "echo $'tab\\there'",
    ];
    let tables: [(&[&str], bool); 3] = [
        (&matching, true),
        (&unresolvable, true),
        (&non_matching, false),
    ];
    for (commands, expected) in tables {
        for command in commands {
            assert_eq!(!guarded_runs(command).is_empty(), expected, "{command}");
        }
    }
}

/// `test_force_with_lease_is_never_a_bare_force`.
#[test]
fn force_with_lease_is_never_a_bare_force() {
    let args = parse_push_args(
        &tokens(&[
            "git",
            "push",
            "--force-with-lease=main:expected",
            "origin",
            "main",
        ]),
        1,
    );
    assert_eq!(
        args,
        PushArgs {
            force: false,
            dry_run: false,
            wildcard: false,
            refspecs: tokens(&["main"]),
            unresolvable: None,
        }
    );
}

/// `test_line_continuations_join_words`.
#[test]
fn line_continuations_join_words() {
    assert_eq!(
        prepared("git push -f origin ma\\\nin"),
        "git push -f origin main"
    );
    assert_eq!(
        prepared("gi\\\nt push -\\\nf origin main"),
        "git push -f origin main"
    );
    assert_eq!(normalize_continuations("echo 'a\\\nb'"), "echo 'a\\\nb'");
}

/// `test_ansi_c_words_decode_like_the_shell` and
/// `test_ansi_c_code_points_are_bounded`.
#[test]
fn ansi_c_words_decode_like_the_shell() {
    for (command, expected) in [
        ("$'git'", "git"),
        ("$'\\x67it'", "git"),
        ("$'\\u0067it'", "git"),
        ("$'\\101BC'", "ABC"),
        ("$\"git\"", "git"),
        ("$'ma\\in'", "main"),
        ("$'\\UFFFFFFFF'", "\u{10FFFF}"),
        ("$'\\U0010FFFF'", "\u{10FFFF}"),
        ("$'\\U0001F600'", "\u{1F600}"),
        ("$'\\u0041BC'", "ABC"),
    ] {
        assert_eq!(values(command), vec![expected.to_string()], "{command}");
    }
}

/// `test_double_quoted_escape_does_not_end_the_string` and
/// `test_redirections_inside_double_quotes_stay_visible`.
#[test]
fn quoted_text_keeps_its_escapes_and_redirections() {
    let command = "echo \"a \\\" b\"";
    let (stripped, _) = strip_escapes(command);
    assert_eq!(stripped.into_iter().collect::<String>(), command);
    assert_eq!(values(command), tokens(&["echo", "a \" b"]));
    assert!(prepared("git push -f origin \" > x\" main").contains("\" > x\""));
}

/// `test_deep_payload_chains_are_refused_by_the_depth_cap`.
#[test]
fn deep_payload_chains_are_refused_by_the_depth_cap() {
    let hides = |payload: &str| {
        payload_hides_force_push(payload, 0, &Budget::unlimited()).expect("unlimited")
    };
    for depth in [2, 3, 5] {
        assert!(
            !hides(&sh_payload_chain(depth, "git status")),
            "depth {depth}"
        );
    }
    for depth in [8, 12] {
        assert!(
            hides(&sh_payload_chain(depth, "git status")),
            "depth {depth}"
        );
        assert!(
            hides(&sh_payload_chain(depth, FORCE_PUSH_LEAF)),
            "depth {depth}"
        );
    }
}

/// `test_url_and_scp_remotes_are_not_refspecs`.
#[test]
fn url_and_scp_remotes_are_not_refspecs() {
    for first in [
        "https://example.invalid/x.git",
        "ssh://example.invalid/x.git",
        "git@github.com:org/repo.git",
        "example.invalid:org/repo.git",
        "localhost:repo.git",
        "myhost:path",
        "origin:main",
        "+main:main",
        "refs/heads/main:refs/heads/main",
        "main:main",
        ":main",
        "C:\\repo",
    ] {
        let args = parse_push_args(&tokens(&["git", "push", "-f", first]), 1);
        assert_eq!(args.refspecs, Vec::<String>::new(), "{first}");
    }
    let args = parse_push_args(&tokens(&["git", "push", "-f", "origin", "main:main"]), 1);
    assert_eq!(args.refspecs, tokens(&["main:main"]));
}

fn flags_all(
    commands: &[String],
    scanner: fn(&str, usize, &Budget) -> super::budget::Scan<bool>,
    expected: bool,
) {
    for command in commands {
        assert_eq!(
            scanner(command, 0, &Budget::unlimited()),
            Ok(expected),
            "{command}"
        );
    }
}

fn owned(commands: &[&str]) -> Vec<String> {
    commands
        .iter()
        .map(|command| (*command).to_string())
        .collect()
}

/// `ForcePushEvalPayloadTest`.
#[test]
fn eval_payloads() {
    let mut hiding = owned(&[
        "eval 'git push -f origin main'",
        "eval \"git push -f origin main\"",
        "eval 'git push --force'",
        "eval 'git push origin +main'",
        "eval 'cd repo && git push -f'",
        "eval 'echo x; git push -f origin main'",
        "eval 'eval \"git push -f origin main\"'",
        "eval 'sh -c \"git push -f origin main\"'",
    ]);
    hiding.push(format!(
        "eval {}",
        json(&sh_payload_chain(3, FORCE_PUSH_LEAF))
    ));
    hiding.push(alternating_payload_chain(5, "eval", FORCE_PUSH_LEAF));
    hiding.push(alternating_payload_chain(15, "eval", FORCE_PUSH_LEAF));
    flags_all(&hiding, eval_payloads_hide_force_push, true);
    let mut safe = owned(&[
        "eval 'git push --force-with-lease origin main'",
        "eval 'git push origin main'",
        "eval 'echo hi'",
        "eval \"echo 'git push -f origin main'\"",
        "eval 'git status'",
    ]);
    safe.push(format!("eval {}", json(&sh_payload_chain(3, "git status"))));
    flags_all(&safe, eval_payloads_hide_force_push, false);
}

/// `ForcePushShellCPayloadTest`.
#[test]
fn shell_c_payloads() {
    let mut hiding = owned(&[
        "sh -c 'git push -f origin main'",
        "bash -c 'git push -f origin main'",
        "bash -lc 'git push --force origin main'",
        "sh -c 'cd repo && git push -f'",
        "sh -c \"eval 'git push -f origin main'\"",
        "sh -c \"env -S 'git push -f origin main'\"",
    ]);
    for depth in [3, 4, 5] {
        hiding.push(sh_payload_chain(depth, FORCE_PUSH_LEAF));
    }
    hiding.push(alternating_payload_chain(5, "sh", FORCE_PUSH_LEAF));
    hiding.push(alternating_payload_chain(15, "sh", FORCE_PUSH_LEAF));
    flags_all(&hiding, shell_c_payloads_hide_force_push, true);
    let mut safe = owned(&[
        "bash -c 'git push --force-with-lease origin main'",
        "bash -c 'echo hi'",
        "bash -c 'echo \"git push -f origin main\"'",
        "bash -c 'git status'",
        "sh -c \"eval 'echo hi'\"",
        "env -S 'sh -c \"git status\"'",
    ]);
    safe.push(sh_payload_chain(2, "git status"));
    safe.push(sh_payload_chain(3, "git status"));
    safe.push(sh_payload_chain(
        3,
        "git push --force-with-lease origin feature",
    ));
    flags_all(&safe, shell_c_payloads_hide_force_push, false);
}

/// `ForcePushEnvPayloadTest`.
#[test]
fn env_payloads() {
    let mut hiding = owned(&[
        "env -S 'git push -f origin main'",
        "env --split-string 'git push -f origin main'",
        "env -iS'git push -f origin main'",
        "env --split-string='git push -f origin main'",
        "env --s 'git push -f origin main'",
        "env --s='git push -f origin main'",
        "env --split 'git push -f origin main'",
        "env -S 'eval \"git push -f origin main\"'",
        "env -S 'sh -c \"git push -f origin main\"'",
        "env --split-string= -S 'git push -f origin main'",
        "env -S '' -S 'git push -f origin main'",
        "env -S'' -S 'git push -f origin main'",
        "env -S -S 'git push -f origin main'",
        "env -S'   ' -S 'git push -f origin main'",
        "env -S '-i' -S 'git push -f origin main'",
    ]);
    hiding.push(format!(
        "env -S {}",
        json(&sh_payload_chain(3, FORCE_PUSH_LEAF))
    ));
    flags_all(&hiding, env_payloads_hide_force_push, true);
    let mut safe = owned(&[
        "env -S 'git status'",
        "env -S 'echo hi'",
        "env -S 'git push --force-with-lease origin feature'",
        "env --split-string 'git status'",
        "env --sp 'git status'",
        "env -C . git status",
        "env VERSION=1 git status",
        "echo env -S",
        "env -S 'echo hi' -S 'git push -f origin main'",
        "env -S -S 'echo hi'",
        "env -S '' 'git push -f origin main'",
    ]);
    safe.push(format!(
        "env -S {}",
        json(&sh_payload_chain(2, "git status"))
    ));
    flags_all(&safe, env_payloads_hide_force_push, false);
}

/// `test_backtick_matcher_follows_bash` (the rule half; the bash-parse half
/// compares against a real bash below).
#[test]
#[cfg_attr(
    not(unix),
    ignore = "runs bash, a POSIX fake git, and real repositories"
)]
fn backtick_matcher_follows_bash() {
    let close = |text: &str| {
        let text: Vec<char> = text.chars().collect();
        matching_backtick(&text, 0, text.len())
    };
    for command in [
        "`printf %s a`",
        "`echo hi`",
        "`printf %s \"a b\"`",
        "`printf %s a; printf %s b`",
    ] {
        let interior: String = command.chars().skip(1).take(close(command) - 1).collect();
        let run = |script: &str| {
            let output = Command::new("bash")
                .arg("-c")
                .arg(script)
                .output()
                .expect("bash runs");
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_string()
        };
        assert_eq!(run(&format!("echo {command}")), run(&interior), "{command}");
    }
    assert_eq!(close("`a`b`"), 2);
    let escaped = "`echo \\``";
    assert_eq!(close(escaped), escaped.chars().count() - 1);
    assert_eq!(close("`echo '` git push -f origin main"), 7);
}

/// `test_parse_tracks_force_dry_run_and_wildcard_with_last_wins`.
#[test]
fn parse_tracks_force_dry_run_and_wildcard_with_last_wins() {
    for (words, expected) in [
        (
            &["git", "push", "--mirror", "origin"][..],
            (true, false, true),
        ),
        (
            &["git", "push", "--all", "origin"][..],
            (false, false, true),
        ),
        (
            &["git", "push", "--mirror", "--no-mirror", "origin"][..],
            (false, false, false),
        ),
        (
            &["git", "push", "--all", "--no-all", "origin"][..],
            (false, false, false),
        ),
        (
            &["git", "push", "-f", "--no-force", "origin", "main"][..],
            (false, false, false),
        ),
        (
            &["git", "push", "--no-force", "-f", "origin", "main"][..],
            (true, false, false),
        ),
        (
            &[
                "git",
                "push",
                "-f",
                "--dry-run",
                "--no-dry-run",
                "origin",
                "main",
            ][..],
            (true, false, false),
        ),
        (
            &["git", "push", "-n", "--no-dry-run", "-f", "origin", "main"][..],
            (true, false, false),
        ),
    ] {
        let args = parse_push_args(&tokens(words), 1);
        assert_eq!(
            (args.force, args.dry_run, args.wildcard),
            expected,
            "{words:?}"
        );
    }
}

/// `ForcePushScanCostTest`: hostile nesting is refused with the budget's or
/// the cap's own message after bounded work, and long benign text is allowed
/// at no nested-scan cost. Cost is the scan's own deterministic work count
/// (the units [`Budget`] charges), not wall-clock time: a timing bound failed
/// under CPU load while the work stayed the same.
#[test]
fn scan_cost_and_budget_verdicts() {
    let sandbox = Sandbox::new();
    let context = sandbox.context();
    let first_line = |command: &str| -> (i64, Option<String>) {
        let (verdict, spent) = check_counting(&Script::bare(command), &context);
        let verdict = verdict
            .err()
            .map(|message| message.lines().next().unwrap_or_default().to_string());
        (spent, verdict)
    };
    // The budget stops the scan on the unit past it.
    let exhausted = Budget::LIMIT + 1;
    let nesting = messages::nesting_refusal()
        .lines()
        .next()
        .map(str::to_string);
    let scan = messages::scan_refusal().lines().next().map(str::to_string);
    let hostile: Vec<(i64, bool)> = [
        substitution_chain(4, 3, FORCE_PUSH_LEAF, '$'),
        substitution_chain(5, 3, FORCE_PUSH_LEAF, '$'),
        substitution_chain(6, 3, FORCE_PUSH_LEAF, '$'),
        substitution_chain(8, 2, FORCE_PUSH_LEAF, '$'),
        substitution_chain(6, 3, "git status", '$'),
        substitution_chain(6, 3, FORCE_PUSH_LEAF, '`'),
        substitution_chain(7, 3, FORCE_PUSH_LEAF, '`'),
    ]
    .iter()
    .map(|command| {
        let (spent, outcome) = first_line(command);
        (
            spent,
            outcome.is_some_and(|line| {
                line.contains("scan budget") || line.contains("Refusing to run")
            }),
        )
    })
    .collect();
    assert_eq!(
        hostile,
        vec![
            (7, true),
            (7, true),
            (7, true),
            (6, true),
            (7, true),
            (2184, true),
            (exhausted, true),
        ]
    );
    let nested: Vec<(i64, Option<String>)> = [4, 6]
        .into_iter()
        .map(|depth| first_line(&nested_substitutions(depth, 3)))
        .collect();
    assert_eq!(nested, vec![(4, nesting.clone()), (4, nesting)]);
    let wide = vec!["$(a)"; 6000].join(" ");
    assert_eq!(first_line(&wide), (exhausted, scan));
    let long_backtick = format!("echo `printf '%s' {}`", "x".repeat(100_000));
    let heredoc = format!(
        "python - <<'EOF'\n{}\nEOF\n",
        (0..1000)
            .map(|index| format!("print({index})"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Length alone costs nothing: 224 KB of flat text spends no unit.
    let benign: Vec<(i64, Option<String>)> = [
        vec!["git log --oneline | head -3"; 8000].join("\n"),
        vec!["echo hello world"; 2000].join("\n"),
        vec!["for f in *.txt; do echo $f; done"; 200].join("\n"),
        heredoc,
        vec!["case $x in a) echo a;; esac"; 500].join("\n"),
        long_backtick,
    ]
    .iter()
    .map(|command| first_line(command))
    .collect();
    assert_eq!(
        benign,
        vec![
            (0, None),
            (0, None),
            (0, None),
            (0, None),
            (0, None),
            (1, None)
        ]
    );
    for command in [
        "echo \"$(git status)\"",
        "X=$(git rev-parse HEAD); echo $X",
        "eval \"$(echo hi)\"",
        "sh -c \"$(echo hi)\"",
        "echo $(echo $(echo $(echo hi)))",
        "eval \"$(eval \"$(eval 'echo hi')\")\" ",
        "git push -f origin $(git rev-parse --abbrev-ref HEAD) branch",
    ] {
        let (_, outcome) = first_line(command);
        assert!(
            !outcome.is_some_and(|line| line.contains("scan budget")),
            "{command}"
        );
    }
}

/// `test_pathological_remote_words_are_classified_quickly` and
/// `test_guard_verdict_for_a_pathological_word_is_still_taken`.
#[test]
fn pathological_remote_words_are_decided_quickly() {
    let bound = Duration::from_millis(500);
    for word in [
        format!("a{}", ".x".repeat(30)),
        format!("a{}/:p", ".x".repeat(30)),
        format!("a{}", "..".repeat(200)),
        format!("a{}:p", "./".repeat(200)),
        "x".repeat(4096),
        format!("a{}/:p", ".x".repeat(2000)),
    ] {
        let started = Instant::now();
        parse_push_args(&tokens(&["git", "push", "-f", &word]), 1);
        assert!(started.elapsed() < bound);
    }
    let sandbox = Sandbox::new();
    let context = sandbox.context();
    let mut refused = 0;
    for command in [
        format!("git push -f origin a{}", ".x".repeat(30)),
        format!("git push -f origin a{}/:p", ".x".repeat(30)),
        format!("git push -f origin a{}", "..".repeat(200)),
        format!("git push -f origin main {}", "x".repeat(4096)),
        format!("git push -f a{}/:p main", ".x".repeat(2000)),
    ] {
        let started = Instant::now();
        refused += usize::from(verdict(&command, &context).is_some());
        assert!(started.elapsed() < bound);
    }
    assert!(refused >= 1);
}

/// `ForcePushGitCommandNameTest`.
#[test]
fn git_subcommands_outside_the_command_table_are_refused() {
    let sandbox = Sandbox::new();
    let context = sandbox.context();
    for command in [
        "git submodule status",
        "git subtree --help",
        "git send-email --help",
        "git daemon --help",
        "git request-pull origin main",
        "git filter-branch --help",
        "git mergetool --help",
        "git merge-octopus --help",
        "git p4 --help",
        "git status",
        "git log --oneline -1",
        "git push --dry-run -f origin main",
    ] {
        assert_eq!(verdict(command, &context), None, "{command}");
    }
    for (command, name) in [
        ("git history", "history"),
        ("git repo", "repo"),
        ("git url-parse", "url-parse"),
        ("git format-rev", "format-rev"),
        ("git last-modified", "last-modified"),
        ("git instaweb", "instaweb"),
        ("git cvsserver --help", "cvsserver"),
        ("git lfs version", "lfs"),
        ("git p", "p"),
        ("git co", "co"),
        ("git st", "st"),
    ] {
        assert_eq!(
            verdict(command, &context),
            Some(messages::git_subcommand_refusal(name)),
            "{command}"
        );
    }
    let message = messages::git_subcommand_refusal("lfs");
    assert!(message.find("Spell out the real subcommand") < message.find("allow_force_push=True"));
}

/// `test_probe_timeout_fails_closed_and_is_event_loop_bounded` and
/// `test_guard_probes_only_on_pattern_match`: a git that never answers makes
/// the implicit-refspec push refuse at the timeout, and a push with no force
/// pattern never probes at all.
#[test]
#[cfg_attr(
    not(unix),
    ignore = "runs bash, a POSIX fake git, and real repositories"
)]
fn a_probe_that_does_not_answer_is_refused_and_plain_pushes_never_probe() {
    assert!(PROBE_TIMEOUT <= Duration::from_secs(2));
    let sandbox = Sandbox::new();
    let bin = sandbox.path().join("bin");
    std::fs::create_dir(&bin).expect("bin dir");
    let git = bin.join("git");
    std::fs::write(&git, "#!/bin/sh\nexec sleep 30\n").expect("fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let context = GuardContext::new(
        sandbox.path(),
        sandbox.env(&format!("{}:/usr/bin:/bin", bin.display())),
    );
    let started = Instant::now();
    assert_eq!(
        check_with(
            &Script::bare("git push -f"),
            &context,
            Duration::from_millis(200)
        )
        .err(),
        Some(messages::probe_timeout_refusal())
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    for command in ["git push origin feature", "echo hi"] {
        assert_eq!(
            check_with(&Script::bare(command), &context, Duration::from_millis(200)),
            Ok(())
        );
    }
}

/// `test_cd_replay_targets_the_right_repo`: the probe runs in the directory
/// the replayed `cd` enters, so the refusal names that repository's upstream.
#[test]
#[cfg_attr(
    not(unix),
    ignore = "runs bash, a POSIX fake git, and real repositories"
)]
fn cd_replay_probes_the_repository_the_push_runs_in() {
    let sandbox = Sandbox::new();
    sandbox.repo("repo-a", "feature");
    sandbox.repo("repo-b", "topic");
    let context = sandbox.context();
    let upstream = |name: &str| {
        messages::refusal(&format!(
            "without a refspec it would force-push the current branch onto its upstream \"{name}\""
        ))
    };
    assert_eq!(
        verdict("cd repo-b && git push -f", &context),
        Some(upstream("origin/topic"))
    );
    assert_eq!(
        verdict("(cd repo-a && git push -f)", &context),
        Some(upstream("origin/feature"))
    );
    // A group that closes before the push never relocates it: the workspace
    // is not a repository, so the guard fails open.
    assert_eq!(
        verdict("(cd repo-b; echo ok) && git push -f", &context),
        None
    );
    assert_eq!(
        verdict("cd repo-b; git push -f", &context),
        Some(messages::relocation_refusal())
    );
}
