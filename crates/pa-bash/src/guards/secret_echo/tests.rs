//! Ports of the assertions in `test_bash_secret_echo_guard.py` that call the
//! private `_secret_echo_violation` helper: same inputs, same expected verdicts.

use super::{violation, Violation};

const MATCHING: &[&str] = &[
    "env",
    "  env  ",
    "printenv",
    "env -0",
    "env -i",
    "env --",
    "printenv -0",
    "export -p",
    "export",
    "export -n",
    "export --",
    "FOO=1 export",
    "echo hi && env",
    "cd /tmp; printenv",
    "env # dump the environment",
    "cat ~/.ssh/id_rsa",
    "cat ~/.ssh/id_ed25519",
    "cat $HOME/.ssh/id_rsa",
    "cat \"$HOME/.ssh/id_rsa\"",
    "cat ${HOME}/.ssh/id_rsa",
    "cat ~/\".ssh\"/id_rsa",
    "cat ~/'.ssh'/id_rsa",
    "cat ~/\".ssh/id_rsa\"",
    "echo ~/'.aws'/credentials",
    "cat ~/'/'.ssh/id_rsa",
    "cat ~/'/'.ssh/'id_rsa'",
    "cat ~/'/'/.ssh/id_rsa",
    "cat ~/.aws/credentials",
    "cat $HOME/.aws/credentials",
    "cat ~/.aws//credentials",
    "cat ~/.aws///credentials",
    "cat $HOME/.aws//credentials",
    "cat ~/.aws/./credentials",
    "cat ~/.aws/../.aws/credentials",
    "cat ~/.aws/cred*",
    "cat ~/.aws/config",
    "cat ~/.gnupg/secring.gpg",
    "echo ~/.gnupg/secring.gpg",
    "FOO=1 env",
    "FOO=1 printenv",
    "FOO=1 export -p",
    "AWS_PROFILE=prod cat ~/.aws/credentials",
    "FOO='bar baz' env",
    "env 2>/dev/null",
    "env 2> /dev/null",
    "export -p 2>&1",
    "env 1>&2",
    "env > /tmp/env.txt",
    "2> /dev/null env",
    "\"env\"",
    "\"cat\" ~/.ssh/id_rsa",
    "ca\"t\" ~/.ssh/id_rsa",
    "cat \"$HOME\"/.ssh/id_rsa",
    "cat $HOME\"/.ssh/id_rsa\"",
    "cat $HOME/\".ssh\"/id_rsa",
    "cat ${HOME}/\".ssh\"/id_rsa",
    "cat $HOME\"\"/.ssh/id_rsa",
    "cat \"$HOME\"\"/.ssh/id_rsa\"",
    "cat \"$HOME/\".ssh\"/id_rsa\"",
    "env -S ''",
    "env -S ' '",
    "env | grep .",
    "env | grep -v SAFE_VAR",
    "env | grep ''",
    "env | grep ^AWS_",
    "env | grep -A5 SAFE_VAR",
    "env | grep -B5 SAFE_VAR",
    "env | grep -C5 SAFE_VAR",
    "env | grep --after-context=5 SAFE_VAR",
    "env 2>&1",
    "env | grep -2 SAFE_VAR",
    "env | grep -10 SAFE_VAR",
    "env 2>&1 | grep -2 PATH",
    "env | grep --context=2 SAFE_VAR",
    "env>&2",
    "env>&1",
    "env>/dev/null",
    "cat>&2 ~/.aws/credentials",
    "env {fd}>/tmp/log",
    "env {fd}>&2",
    "printenv {fd}>/tmp/log",
    "export -p {fd}>log",
    "env &>/dev/null",
    "env &>log",
    "env &>>log",
    "env -0 &>log",
    "printenv &>/dev/null",
    "&>log cat ~/.ssh/id_rsa",
    "$'env'",
    "e$'nv'",
    "c$'at' ~/.ssh/id_rsa",
    "$'cat' ~/.aws/credentials",
    "$\"env\"",
    "echo \"$(env)\"",
    "echo \"$(cat ~/.ssh/id_rsa)\"",
    "echo \"`env`\"",
    "echo \"$(printenv)\"",
    "echo $(( $(env) ))",
    "echo \"$(( $(env) ))\"",
    "echo $(( $(cat ~/.ssh/id_rsa) ))",
    "echo $( (env) )",
    "echo $((env) )",
    "echo $(( env) )",
    "echo $((env ) )",
    "echo $((printenv) )",
    "echo 1$((env) )",
    "cat <<EOF\n$(env)\nEOF",
    "echo \"a <<'EOF' b\"\nenv",
    "cat <<'$(env)'\n$(env)\n$(env)",
    "echo \\\\$HOME/.ssh/id_rsa",
    "env>&2 | grep SAFE_VAR",
    "env >&2 | grep SAFE_VAR",
    "env 1>&2 | grep SAFE_VAR",
    "env>&2|grep PATH",
    "env | grep -- -v SAFE_VAR",
    "env |\nenv",
    "env |\n\nenv",
    "env cat ~/.ssh/id_rsa",
    "env echo ~/.ssh/id_rsa",
    "env cat $HOME/.aws/credentials",
    "env cat ~/'/'.ssh/id_rsa",
    "env -S 'cat ${HOME}/.ssh/id_rsa'",
    "env | grep -10i SAFE_VAR",
    "env | grep -2i SAFE_VAR",
    "env | grep -i2 SAFE_VAR",
    "env | grep -F2 SAFE_VAR",
    "env | grep --cont=2 SAFE_VAR",
    "env | grep --after-c=2 SAFE_VAR",
    "env | grep -1m PATH",
    "echo $(env) # hi",
    "env >&\"2\" | grep SAFE_VAR",
    "env >&'2' | grep SAFE_VAR",
    "env >&$'2' | grep SAFE_VAR",
    "env >&\\2 | grep SAFE_VAR",
    "env >&${X} | grep SAFE_VAR",
    "env >&`printf 2` | grep SAFE_VAR",
    "env 2>&1 1>&2 | grep SAFE_VAR",
    "env >&2x | grep SAFE_VAR",
    "cat <<'A'\nenv\nA | grep x",
    "(( x = 1 << 2 ))\nenv\n2",
    "(( x = 1 << 2 ))\ncat ~/.ssh/id_rsa\n2",
    "echo $((1<<y))\nenv\ny))",
    "$[ 1 << 2 ]\nenv\n2",
    "env | grep -m0 SAFE_VAR",
    "env | grep -m00 SAFE_VAR",
    "env | grep -m 0 SAFE_VAR",
    "env | grep --max-count=0 SAFE_VAR",
    "env | grep --max-count 0 SAFE_VAR",
    "env >log | grep SAFE_VAR",
    "env &>log | grep SAFE_VAR",
    "env >>log | grep SAFE_VAR",
    "env >/dev/null | grep SAFE_VAR",
    "env | grep --max-c=0 SAFE_VAR",
    "env | grep --max=0 SAFE_VAR",
    "env | grep --ma=0 SAFE_VAR",
    "env | grep --max-c=00 SAFE_VAR",
    "env 1<>/dev/stderr | grep SAFE_VAR",
    "env 1<>/dev/fd/2 | grep SAFE_VAR",
    "env <>log | grep SAFE_VAR",
    "env | grep -z SAFE_VAR",
    "env | grep -z PATH",
    "env | grep --null-data SAFE_VAR",
    "env | grep -z -m1 PATH",
    "env -u PATH",
    "env -u PATH OTHER=1",
    "env FOO=1",
    "env printenv",
    "env -u PATH printenv",
    "env FOO=1 printenv",
    "env env",
    "env -S 'env'",
    "env -S env",
    "env -i printenv",
    "echo \"$( #)\nenv )\"",
    "echo \"$( #)\nprintenv )\"",
    "echo \"$(#)\nenv )\"",
    "echo \"$( : # )\ncat ~/.ssh/id_rsa )\"",
    "echo \"$(# )\ncat ~/.ssh/id_rsa )\"",
];

const NON_MATCHING: &[&str] = &[
    "ls ~/.ssh",
    "ls -la ~/.aws",
    "ls ~/.gnupg",
    "printenv HOME",
    "printenv PATH SAFE_VAR",
    "env FOO=1 cmd",
    "env -u FOO cmd",
    "printenv -0 FOO",
    "printenv -l FOO",
    "export FOO=1",
    "export -n FOO",
    "export FOO",
    "export -f",
    "cat .env",
    "grep GITHUB_TOKEN .env",
    "env | grep SAFE_VAR",
    "printenv | grep SAFE_VAR",
    "export -p | grep SAFE_VAR",
    "grep GITHUB_TOKEN ~/.aws/credentials",
    "echo 'env'",
    "echo 'cat ~/.ssh/id_rsa'",
    "echo \"cat ~/.ssh/id_rsa\"",
    "cat \"~/.ssh/id_rsa\"",
    "cat '~/.ssh/id_rsa'",
    "echo \"~/.ssh/id_rsa\"",
    "env cat .env",
    "env cat README.md",
    "env echo hi",
    "env head ~/.ssh/id_rsa",
    "env -S 'cat ~/.ssh/id_rsa'",
    "env cat",
    "cat '~'/.ssh/id_rsa",
    "cat ~\"/\".ssh/id_rsa",
    "cat ~\"/\".ssh/\"/\"id_rsa",
    "cat ~\"\"/.ssh/id_rsa",
    "cat ~'/'.ssh/id_rsa",
    "cat '$HOME/.ssh/id_rsa'",
    "cat /etc/passwd",
    "cat README.md",
    "echo $HOME",
    "git status",
    "npm run check",
    "env 'foo&bar'",
    "echo \"a & env\"",
    "\"env -0\"",
    "env | grep -e SAFE_VAR",
    "env | grep -- SAFE_VAR",
    "env 2>/dev/null | grep PATH",
    "env | grep -F SAFE_VAR",
    "env | grep -i PATH",
    "env | grep -a PATH",
    "env 2>&1 | grep PATH",
    "printenv 2>&1 | grep SAFE_VAR",
    "env | grep SAFE_VAR &>/dev/null",
    "env | grep SAFE_VAR &>log",
    "env | grep -- -v",
    "env |\ngrep PATH",
    "env |\ngrep -m1 PATH",
    "env |\n grep PATH",
    "echo $((env))",
    "echo \"$((env))\"",
    "echo $(( (1+2) * 3 ))",
    "env -S 'printenv HOME'",
    "echo $((env)) | cat",
    "echo $(( 1 )) ; env | grep SAFE_VAR",
    "env -S 'printenv PATH'",
    "env | grep -m1 PATH",
    "env | grep -F -m1 PATH",
    "env | grep -im1 PATH",
    "env | grep -m10 SAFE_VAR",
    "env2>&1",
    "cat <<'EOF'\nenv\nEOF",
    "cat <<\"EOF\"\nenv\nEOF",
    "cat <<-'EOF'\n\tenv\n\tEOF",
    "cat <<'EOF'\ncat ~/.ssh/id_rsa\nEOF",
    "cat <<'$(env)'\nhello\n$(env)",
    "cat <<-'$(env)'\n\thello\n\t$(env)",
    "cat <<EOF\nenv\nEOF",
    "cat <<EOF\ncat ~/.ssh/id_rsa\nEOF",
    "cat <<EOF\nexport -p\nEOF",
    "echo $'env'",
    "echo \"$'env'\"",
    "echo $'\\c\u{df}'",
    "echo \"$(env | grep SAFE_VAR)\"",
    "echo \"$(printenv HOME)\"",
    "echo '$(env)'",
    "echo \"$(cat /etc/passwd)\"",
    "echo \\~/.ssh/id_rsa",
    "echo \\$HOME/.ssh/id_rsa",
    "echo \"\\$HOME/.ssh/id_rsa\"",
    "echo a\\;env",
    "cat \\$HOME/.aws/credentials",
    "env>&1 | grep SAFE_VAR",
    "env >&1 | grep PATH",
    "env | grep --fixed-strings SAFE_VAR",
    "echo hi # $(env)",
    "echo hi # `env`",
    "echo \"x\" # $(env)",
    "echo \"$( #)\nprintf OK )\"",
    "echo \"$( #)\ncat /etc/hosts )\"",
    "env '>&2' | grep SAFE_VAR",
    "env \">&2\" | grep SAFE_VAR",
    "env | grep --contextual SAFE_VAR",
    "let a=1<<2\nenv\n2",
    "env | grep -m 1 SAFE_VAR",
    "env | grep --max-count 1 SAFE_VAR",
    "env | grep -m PATH",
    "env | grep --max-count= SAFE_VAR",
    "env | grep --max-c=1 SAFE_VAR",
    "env 0<>/dev/stderr | grep SAFE_VAR",
    "env -C /tmp ls",
    "env -S 'ls -l'",
    "env printenv HOME",
    "env -u FOO printenv PATH",
    "env | grep -c PATH",
    "env | grep -n PATH",
    "env | grep -o PATH",
    "cat <<EOF\n(( 1 << 2 ))\nEOF",
];
/// `test_matches_dumps_and_secret_file_reads`.
#[test]
fn matches_dumps_and_secret_file_reads() {
    let missed: Vec<&str> = MATCHING
        .iter()
        .copied()
        .filter(|command| violation(command).is_none())
        .collect();
    assert_eq!(missed, Vec::<&str>::new());
}

/// `test_does_not_match_targeted_or_literal_commands`.
#[test]
fn does_not_match_targeted_or_literal_commands() {
    let refused: Vec<&str> = NON_MATCHING
        .iter()
        .copied()
        .filter(|command| violation(command).is_some())
        .collect();
    assert_eq!(refused, Vec::<&str>::new());
}

/// `test_cost_locks_keep_deep_scans_bounded`: interiors come from a
/// worklist, so nesting depth cannot exhaust the stack, and a dump at any
/// depth is a dump.
#[test]
fn deep_scans_stay_bounded() {
    let nested = format!("{}echo hi{}", "\"$(".repeat(1200), ")".repeat(1200));
    let chain = "\"$( ".repeat(2000);
    assert_eq!(violation(&nested), None);
    assert_eq!(violation(&chain), None);
    assert_eq!(
        violation(&format!("{}env{}", "\"$(".repeat(1200), ")".repeat(1200))),
        Some(Violation::FullEnvironment)
    );
    assert_eq!(
        violation(&format!("{chain}env")),
        Some(Violation::FullEnvironment)
    );
}

/// `test_cost_lock_keeps_an_unterminated_heredoc_linear`.
#[test]
fn an_unterminated_heredoc_still_reads_its_body_words() {
    assert_eq!(
        violation(&"cat <<'EOF'\nenv\n".repeat(4000)),
        Some(Violation::FullEnvironment)
    );
}

/// `test_cost_lock_keeps_many_heredoc_openers_linear`.
#[test]
fn many_heredoc_openers_on_one_line_run_nothing() {
    assert_eq!(violation(&format!("cat {}body", "<<A ".repeat(8000))), None);
}

/// `test_nested_substitution_inner_command_still_scanned`.
#[test]
fn a_deeply_nested_inner_command_is_still_scanned() {
    let nested = |inner: &str| format!("{}{inner}{}", "\"$(echo ".repeat(200), ")\"".repeat(200));
    assert_eq!(
        violation(&nested("\"$(env)\"")),
        Some(Violation::FullEnvironment)
    );
    assert_eq!(
        violation(&nested("\"$(cat ~/.ssh/id_rsa)\"")),
        Some(Violation::SecretFile)
    );
    assert_eq!(violation(&nested("env")), None);
}

/// The spaced pattern values of `test_glued_grep_pattern_refused`.
#[test]
fn a_spaced_grep_pattern_keeps_the_filtered_read() {
    assert_eq!(violation("env | grep -e SAFE_VAR"), None);
    assert_eq!(violation("env | grep --regexp SAFE_VAR"), None);
}

/// `test_multibyte_control_escape_answers_a_verdict`.
#[test]
fn a_multibyte_control_escape_answers_a_verdict() {
    assert_eq!(violation("echo $'\\cß'"), None);
}

/// `test_overlong_descriptor_returns_a_verdict`.
#[test]
fn an_overlong_descriptor_fails_closed() {
    let command = format!("env {}>&1 | grep SAFE_VAR", "9".repeat(4500));
    assert_eq!(violation(&command), Some(Violation::FullEnvironment));
}
