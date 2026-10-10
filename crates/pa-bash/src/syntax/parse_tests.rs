use super::*;
use crate::syntax::ast::{Command, Part, Quoting};

fn simple(text: &str) -> Vec<Vec<String>> {
    let parsed = parse(text);
    let mut out = Vec::new();
    collect(&parsed.list, &mut out);
    out
}

fn collect(list: &List, out: &mut Vec<Vec<String>>) {
    for item in &list.items {
        for pipeline in item.pipelines() {
            for command in &pipeline.commands {
                match command {
                    Command::Simple(simple) => out.push(
                        simple
                            .words
                            .iter()
                            .map(|word| {
                                word.as_static()
                                    .unwrap_or_else(|| format!("<{}>", word.raw))
                            })
                            .collect(),
                    ),
                    Command::Subshell(list, _) | Command::Group(list, _) => collect(list, out),
                    Command::Branches(lists, _) => {
                        for list in lists {
                            collect(list, out);
                        }
                    }
                    Command::For { body, .. } => collect(body, out),
                    Command::Case { arms, .. } => {
                        for (_, body) in arms {
                            collect(body, out);
                        }
                    }
                    Command::Function { body, .. } => {
                        let list = List {
                            items: vec![single(*body.clone())],
                        };
                        collect(&list, out);
                    }
                    Command::Conditional(_) | Command::Arithmetic(_) | Command::Unparsed(_) => {}
                }
            }
        }
    }
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_string()).collect()
}

#[test]
fn here_string_operators_inside_single_quotes_are_text() {
    // The lexer quoting bug: `<<<` inside single quotes was read as a
    // here-string operator.
    let parsed = parse("git grep -n -E '^(<<<<<<<|>>>>>>>)' -- src");
    assert!(parsed.complete);
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple command");
    };
    assert!(command.redirects.is_empty());
    assert_eq!(
        command.words[4].parts,
        vec![Part::Literal(
            "^(<<<<<<<|>>>>>>>)".to_string(),
            Quoting::Single
        )]
    );
}

#[test]
fn lists_pipelines_and_connectors() {
    assert_eq!(
        simple("a b && c | d; e & f || g\nh"),
        vec![
            words(&["a", "b"]),
            words(&["c"]),
            words(&["d"]),
            words(&["e"]),
            words(&["f"]),
            words(&["g"]),
            words(&["h"])
        ]
    );
}

#[test]
fn quoting_is_removed_and_kept_per_part() {
    assert_eq!(
        simple(r#"g"i"t 'pu'sh \-f $'\x2d'f"#),
        vec![words(&["git", "push", "-f", "-f"])]
    );
    let parsed = parse(r#"echo "a $x `b` $(c d)" e"#);
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple");
    };
    assert_eq!(command.words.len(), 3);
    assert!(command.words[1].has_expansion());
}

#[test]
fn case_patterns_are_not_commands() {
    assert_eq!(
        simple("case $w in a|b) echo keep;; *) rm x;; esac; ls"),
        vec![
            words(&["echo", "keep"]),
            words(&["rm", "x"]),
            words(&["ls"])
        ]
    );
}

#[test]
fn heredoc_bodies_attach_to_their_redirect() {
    let parsed = parse("cat <<'EOF' > out\nsudo id\nEOF\necho after");
    assert!(parsed.complete);
    assert_eq!(parsed.list.items.len(), 2);
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple");
    };
    let heredoc = command.redirects[0].heredoc.as_ref().expect("heredoc");
    assert_eq!(heredoc.raw, "sudo id\n");
    let parsed = parse("cat <<-A <<B\n\tone\n\tA\ntwo\nB\nnext");
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple");
    };
    assert_eq!(
        command.redirects[0]
            .heredoc
            .as_ref()
            .map(|h| h.raw.as_str()),
        Some("one\n")
    );
    assert_eq!(
        command.redirects[1]
            .heredoc
            .as_ref()
            .map(|h| h.raw.as_str()),
        Some("two\n")
    );
    assert_eq!(
        simple("cat <<-A <<B\n\tone\n\tA\ntwo\nB\nnext")[1],
        words(&["next"])
    );
}

#[test]
fn unquoted_heredoc_bodies_parse_substitutions() {
    let parsed = parse("cat <<EOF\n$(git push -f)\nEOF");
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple");
    };
    let body = &command.redirects[0].heredoc.as_ref().expect("heredoc").body;
    assert!(body
        .parts
        .iter()
        .any(|part| matches!(part, Part::Command { .. })));
}

#[test]
fn substitutions_nest() {
    fn walk(word: &Word, found: &mut Vec<String>) {
        for part in &word.parts {
            match part {
                Part::Command { body, .. } | Part::Process { body, .. } => {
                    let mut out = Vec::new();
                    collect(body, &mut out);
                    for command in &out {
                        found.push(command.join(" "));
                    }
                    crate::syntax::walk::visit_words(body, &mut |word| walk(word, found));
                }
                Part::Parameter {
                    operator: Some(operator),
                    ..
                } => walk(operator, found),
                Part::Arithmetic(word) => walk(word, found),
                Part::Parameter { .. } | Part::Literal(..) => {}
            }
        }
    }
    let parsed = parse("echo $(a $(b) `c`) <(d) ${x:-$(e)} $((1 + $(f)))");
    assert!(parsed.complete);
    let mut found = Vec::new();
    crate::syntax::walk::visit_words(&parsed.list, &mut |word| walk(word, &mut found));
    found.sort();
    assert_eq!(
        found,
        vec!["<$(b)> <`c`>", "a <$(b)> <`c`>", "b", "c", "d", "e", "f"]
            .into_iter()
            .filter(|s| !s.starts_with('<'))
            .map(String::from)
            .collect::<Vec<_>>()
    );
}

#[test]
fn compound_commands() {
    assert_eq!(
        simple("if a; then b; elif c; then d; else e; fi; while f; do g; done; for x in 1 2; do h $x; done; { i; }; (j); f() { k; }"),
        vec![
            words(&["a"]), words(&["b"]), words(&["c"]), words(&["d"]), words(&["e"]),
            words(&["f"]), words(&["g"]), vec!["h".to_string(), "<$x>".to_string()],
            words(&["i"]), words(&["j"]), words(&["k"]),
        ]
    );
}

#[test]
fn assignments_and_arrays() {
    let parsed = parse("a=($(rd)) B=1 cmd x=2");
    let Command::Simple(command) = &parsed.list.items[0].first.commands[0] else {
        panic!("simple");
    };
    assert_eq!(command.assignments.len(), 2);
    assert_eq!(command.words.len(), 2);
    assert_eq!(command.words[1].as_static().as_deref(), Some("x=2"));
}

#[test]
fn conditionals_keep_their_operators_as_words() {
    assert_eq!(
        simple("[[ $a =~ ^(x|y)$ && -f b ]] && echo ok"),
        vec![words(&["echo", "ok"])]
    );
    assert!(parse("[[ $a =~ ^(x|y)$ && -f b ]] && echo ok").complete);
}

#[test]
fn malformed_input_still_yields_what_bash_would_run() {
    // A syntax error drops its whole line and what follows; earlier lines
    // run.
    let parsed = parse("echo a\necho hi; echo 'unterminated");
    assert!(!parsed.complete);
    assert_eq!(
        simple("echo a\necho hi; echo 'unterminated"),
        vec![words(&["echo", "a"])]
    );
    assert_eq!(
        parsed.dropped.as_deref(),
        Some("echo hi; echo 'unterminated")
    );
    assert!(!parse("echo )").complete);
    assert_eq!(simple("ls\nrg -o (a|b) x; set"), vec![words(&["ls"])]);
    assert_eq!(simple("fi; ls"), Vec::<Vec<String>>::new());
}

#[test]
fn nesting_past_the_bound_is_kept_unparsed() {
    let deep = format!("{}chmod -R 777 /{}", "$(".repeat(200), ")".repeat(200));
    let parsed = parse(&deep);
    assert!(parsed.dropped.is_none());
    assert!(parsed.list.visible().contains("chmod -R 777 /"));
}

/// Parser steps for `text`.
pub(crate) fn work(text: &str) -> usize {
    WORK.with(|work| work.set(0));
    let _ = parse(text);
    WORK.with(std::cell::Cell::get)
}

#[test]
fn linear_in_the_text() {
    // Eight times the input costs about eight times the steps (a quadratic
    // scan would cost sixty-four).
    for (name, make) in [
        (
            "openers",
            (|n: usize| format!("cat {}body", "<<A ".repeat(n))) as fn(usize) -> String,
        ),
        ("unterminated", |n| "cat <<'EOF'\nenv\n".repeat(n)),
        ("lists", |n| "echo $(a) \"$b\" 'c' | d && e; ".repeat(n)),
        ("nesting", |n| {
            format!("{}x{}", "$(".repeat(60), ")".repeat(60)).repeat(n / 60 + 1)
        }),
        ("deep", |n| format!("{}x{}", "$(".repeat(n), ")".repeat(n))),
        ("quotes", |n| "\"".repeat(n)),
        ("braces", |n| "${".repeat(n)),
    ] {
        let small = work(&make(1000)).max(1);
        let large = work(&make(8000));
        assert!(large < small * 12, "{name}: {large} steps against {small}");
    }
}

#[test]
fn a_dollar_before_a_closing_quote_is_literal() {
    let parsed = parse(r#"grep -cE "^test .* ok$" | grep -v "^--$""#);
    assert!(parsed.complete);
    assert_eq!(
        simple(r#"grep -v "^--$""#),
        vec![words(&["grep", "-v", "^--$"])]
    );
}

/// Debug aid: `PA_BASH_PARSE_PROBE=<jsonl with "command">` prints every
/// command bash would accept that the parser drops text from.
#[test]
#[ignore = "a manual probe over a private command corpus"]
fn probe_corpus_for_dropped_text() {
    let Ok(path) = std::env::var("PA_BASH_PARSE_PROBE") else {
        return;
    };
    let text = std::fs::read_to_string(path).expect("corpus");
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("json");
        let command = value["command"].as_str().unwrap_or_default();
        let parsed = parse(command);
        if let Some(dropped) = parsed.dropped {
            let at = command.len() - dropped.len();
            println!(
                "{}\n  >> {:?}\n",
                command.len(),
                &command[at..(at + 160).min(command.len())]
            );
        }
    }
}

#[test]
fn declaration_builtins_take_compound_assignments() {
    let parsed = parse("declare -A urls=(\n  [\"a\"]=\"https://x\"\n  [b]=$(c)\n)\nfor k in \"${!urls[@]}\"; do curl -s \"${urls[$k]}\"; done");
    assert!(parsed.complete);
    assert_eq!(
        simple("declare -A m=( [k]=v ); echo ok")[1],
        words(&["echo", "ok"])
    );
}

#[test]
fn heredocs_inside_substitutions_and_function_subshells() {
    let parsed =
        parse("eval \"$(cat <<EOF\nchmod -R 755 ~\nEOF)\"; function f (chmod \"$@\"); f -R 755 ~");
    assert!(parsed.complete, "{parsed:?}");
    assert!(parsed.list.visible().contains("chmod -R 755 ~"));
    assert_eq!(simple("function f (chmod x); f a")[1], words(&["f", "a"]));
}

#[test]
fn ansi_c_escapes_before_multibyte_chars_stay_on_char_boundaries() {
    for text in ["echo $'\\cß'", "echo $'\\ß'", "echo $'\\c'", "echo $'\\x'"] {
        let _ = parse(text);
    }
}
