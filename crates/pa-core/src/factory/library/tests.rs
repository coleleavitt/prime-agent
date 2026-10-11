//! The machine library's behaviours, pinned to what the original Python
//! library (`rlm.factory` under the kernel's Python 3.11) produced for the
//! same inputs. The runtime's own battery (`test_factory.py`'s machine
//! library classes) runs unchanged against this implementation through the
//! kernel client.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::machine_file::dumps_pretty;
use super::pyfs::Fs;
use super::pyjson::{self, LoadError};
use super::*;

fn py(value: &Value) -> PyValue {
    PyValue::from_json(value)
}

fn decode_error(text: &str) -> String {
    match pyjson::loads(text) {
        Err(LoadError::Decode(message)) => message,
        other => panic!("{text:?} decoded to {other:?}"),
    }
}

#[test]
fn json_errors_are_cpythons_messages_and_positions() {
    for (text, message) in [
        ("", "Expecting value: line 1 column 1 (char 0)"),
        ("[1,]", "Expecting value: line 1 column 4 (char 3)"),
        (
            "{\"a\":1,}",
            "Expecting property name enclosed in double quotes: line 1 column 8 (char 7)",
        ),
        (
            "{\"a\" 1}",
            "Expecting ':' delimiter: line 1 column 6 (char 5)",
        ),
        ("nul", "Expecting value: line 1 column 1 (char 0)"),
        ("-", "Expecting value: line 1 column 1 (char 0)"),
        ("01", "Extra data: line 1 column 2 (char 1)"),
        ("1.", "Extra data: line 1 column 2 (char 1)"),
        (
            "\"\\u12\"",
            "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
        ),
        ("\"\\x\"", "Invalid \\escape: line 1 column 2 (char 1)"),
        (
            "\"a\nb\"",
            "Invalid control character at: line 1 column 3 (char 2)",
        ),
        (
            "\"unterminated",
            "Unterminated string starting at: line 1 column 1 (char 0)",
        ),
        (
            "\"ab\\\"",
            "Unterminated string starting at: line 1 column 1 (char 0)",
        ),
        ("[1 2]", "Expecting ',' delimiter: line 1 column 4 (char 3)"),
        ("  [ ]  x", "Extra data: line 1 column 8 (char 7)"),
        (
            "\u{feff}{}",
            "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)",
        ),
        ("{\"a\"\n:\n}", "Expecting value: line 3 column 1 (char 7)"),
        // Positions count code points, not bytes.
        (
            "[\"é\" 1]",
            "Expecting ',' delimiter: line 1 column 6 (char 5)",
        ),
    ] {
        assert_eq!(decode_error(text), message, "{text:?}");
    }
    assert_eq!(
        pyjson::loads(&"1".repeat(4301)),
        Err(LoadError::Value(
            "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 \
             digits; use sys.set_int_max_str_digits() to increase the limit"
                .to_string()
        ))
    );
    let deep = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
    assert_eq!(
        pyjson::loads(&deep),
        Err(LoadError::Recursion(
            "maximum recursion depth exceeded while decoding a JSON array from a unicode string"
                .to_string()
        ))
    );
}

#[test]
fn json_values_keep_python_semantics() {
    let value = pyjson::loads(
        "{\"a\": 1, \"b\": [NaN, -Infinity, 1e400, 123456789012345678901234567890123456789012], \
         \"a\": {\"\\ud83d\\ude00\": -0.0}}",
    )
    .expect("decodes");
    let PyValue::Dict(pairs) = &value else {
        panic!("{value:?}");
    };
    // A repeated key keeps its first position with the last value.
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].0, PyValue::Str("a".into()));
    assert_eq!(
        pairs[0].1,
        PyValue::Dict(vec![(PyValue::Str("😀".into()), PyValue::Float(-0.0))])
    );
    let PyValue::List(items) = &pairs[1].1 else {
        panic!("{pairs:?}");
    };
    assert!(matches!(items[0], PyValue::Float(nan) if nan.is_nan()));
    assert_eq!(items[1], PyValue::Float(f64::NEG_INFINITY));
    assert_eq!(items[2], PyValue::Float(f64::INFINITY));
    assert_eq!(
        items[3],
        PyValue::BigInt("123456789012345678901234567890123456789012".into())
    );
    // `json.dumps((1, "é", float("nan"), {"k": [True, None]}))`.
    let tuple_like = py(&json!([1, "é", null, {"k": [true, null]}]));
    assert_eq!(
        pyjson::dumps(&tuple_like),
        Ok("[1, \"\\u00e9\", null, {\"k\": [true, null]}]".to_string())
    );
}

fn machine_text(frontmatter: &str, body: &str) -> String {
    format!("{frontmatter}\n\n{body}")
}

const FRONTMATTER: &str =
    "---\nname: sweep\ndescription: A machine that sweeps.\nversion: 1\nauthor: Tester\n---";
const SPEC: &str = "{\"run\": {\"failure_policy\": \"continue\"}, \"states\": [{\"id\": \"a\", \"entry\": true, \"subagent\": {\"prompt\": \"Do the work.\"}}]}";

fn parse(text: &str) -> (Option<MachineFile>, Vec<String>) {
    parse_machine_file(text, "test-MACHINE.md").expect("parses")
}

#[test]
fn a_machine_file_parses_frontmatter_and_one_spec_fence() {
    let text = machine_text(
        FRONTMATTER,
        &format!(
            "# sweep\n\n```json\n{{\"not\": \"a machine\"}}\n```\n\n```machine-spec\n{SPEC}\n```"
        ),
    );
    let (machine, errors) = parse(&format!("\u{feff}{}", text.replace('\n', "\r\n")));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        machine,
        Some(MachineFile {
            name: "sweep".into(),
            description: "A machine that sweeps.".into(),
            version: "1".into(),
            author: "Tester".into(),
            spec: pyjson::loads(SPEC).expect("spec"),
        })
    );
    let quoted = "---\nname: \"sweep\"\ndescription: 'It: reviews ''things''.'\n---";
    let (machine, errors) = parse(&machine_text(
        quoted,
        &format!("```machine-spec\n{SPEC}\n```"),
    ));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        machine.map(|machine| machine.description),
        Some("It: reviews 'things'.".to_string())
    );
}

#[test]
fn malformed_machine_files_name_every_defect() {
    let spec_body = format!("```machine-spec\n{SPEC}\n```");
    for (text, errors) in [
        (
            "# just prose".to_string(),
            vec!["test-MACHINE.md: MACHINE.md must start with a `---` frontmatter block"],
        ),
        (
            "---\nname: sweep\nno close".to_string(),
            vec!["test-MACHINE.md: frontmatter is not closed (end it with a `---` line)"],
        ),
        (
            machine_text(
                "---\nname: sweep\nname: again\n\nlicense: MIT\nbare\ndescription: Reviews: all\nversion:\n---",
                &spec_body,
            ),
            vec![
                "test-MACHINE.md: frontmatter field 'name' is declared more than once",
                "test-MACHINE.md: frontmatter line 4 is empty (one `key: value` line per field)",
                "test-MACHINE.md: unknown frontmatter key 'license' (allowed: name, description, version, author)",
                "test-MACHINE.md: frontmatter line 6 must be `key: value`",
                "test-MACHINE.md: frontmatter description is not a plain scalar (quote the value to include ':' characters)",
                "test-MACHINE.md: frontmatter field 'version' requires a value",
            ],
        ),
        (
            machine_text("---\ndescription: \"bad \\x\"\n---", &spec_body),
            vec![
                "test-MACHINE.md: frontmatter description has an invalid double-quoted value (Invalid \\escape: line 1 column 6 (char 5))",
            ],
        ),
        (
            machine_text(FRONTMATTER, "No spec here."),
            vec![
                "test-MACHINE.md: MACHINE.md requires exactly one fenced ```machine-spec block; found none",
            ],
        ),
        (
            machine_text(FRONTMATTER, &format!("{spec_body}\n{spec_body}")),
            vec![
                "test-MACHINE.md: MACHINE.md requires exactly one fenced ```machine-spec block; found 2",
            ],
        ),
        (
            machine_text(FRONTMATTER, "```machine-spec\n{}"),
            vec!["test-MACHINE.md: the ```machine-spec fence is never closed"],
        ),
        (
            machine_text(FRONTMATTER, "```text\nnever closed"),
            vec!["test-MACHINE.md: the ```text fence opened at line 2 is never closed"],
        ),
        (
            machine_text(FRONTMATTER, "```machine-spec\n[1, 2]\n```"),
            vec![
                "test-MACHINE.md: the ```machine-spec block must contain a JSON object, got a list",
            ],
        ),
        (
            machine_text(FRONTMATTER, "```machine-spec\nnot json\n```"),
            vec![
                "test-MACHINE.md: the ```machine-spec block must contain a JSON object (Expecting value: line 1 column 1 (char 0))",
            ],
        ),
        (
            machine_text(
                "---\nname: Sweep-\ndescription: \"two\\nlines\"\n---",
                &spec_body,
            ),
            vec![
                "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)",
                "machine name must not end with a hyphen",
                "frontmatter description must be a single line",
            ],
        ),
    ] {
        let (machine, found) = parse(&text);
        assert_eq!(machine, None, "{text:?}");
        assert_eq!(found, errors, "{text:?}");
    }
}

#[test]
fn name_and_description_rules_mirror_the_skill_library() {
    assert_eq!(
        machine_name_errors(&py(&json!("sweep-2"))),
        Vec::<String>::new()
    );
    for bad in [json!(""), json!(7), json!(null)] {
        assert_eq!(
            machine_name_errors(&py(&bad)),
            ["machine name must be a non-empty string"]
        );
    }
    assert_eq!(
        machine_name_errors(&py(&json!("a".repeat(65)))),
        ["machine name exceeds 64 characters (65)"]
    );
    assert_eq!(
        machine_description_errors(&py(&json!(" \u{1c} "))),
        ["frontmatter description is required"]
    );
    assert_eq!(
        machine_description_errors(&py(&json!("d".repeat(1025)))),
        ["frontmatter description exceeds 1024 characters (1025)"]
    );
}

#[test]
fn a_rendered_machine_is_the_original_renderers_bytes() {
    let spec = py(&json!({
        "run": {"failure_policy": "continue", "max_parallel": 2, "budget_ms": 900_000},
        "states": [
            {"id": "collect", "entry": true, "subagent": {"prompt": "Collect the findings for the branch."}, "outputs": [{"name": "findings", "type": "text"}]},
            {"id": "review", "subagent": "reviewer", "max_entries": 3, "inputs": [{"name": "draft", "type": "text", "from": "collect.findings", "optional": true}], "foreach": {"over": "draft", "max": 4}},
            {"id": "watch", "lifecycle": "resident", "subagent": {"name": "watcher", "prompt": "Stay."}}
        ],
        "transitions": [
            {"from": "collect", "to": "review"},
            {"from": ["collect", "review"], "to": "watch", "when": {"output": "verdict", "path": "approved", "op": "eq", "value": "é"}}
        ]
    }));
    let fence = dumps_pretty(&spec).expect("dumps");
    let text = render_machine_file(&RenderFields {
        name: &py(&json!("sweep")),
        description: &py(&json!("Reviews: everything.")),
        version: &py(&json!("1")),
        author: &py(&json!("Prime Agent")),
        spec: &spec,
        spec_json: Ok(&fence),
    })
    .expect("renders");
    let expected = "---\nname: sweep\ndescription: \"Reviews: everything.\"\nversion: 1\nauthor: Prime Agent\n---\n\n# sweep\n\n## Machine contract\n\nRun: failure_policy=continue, max_parallel=2, budget_ms=900000\n\nStates:\n- collect (entry)\n  subagent: inline (Collect the findings for the branch.)\n  output: findings (text)\n- review (max_entries=3)\n  subagent: reviewer\n  input: draft (text) <- collect.findings [optional]\n  foreach: over draft, max 4\n- watch (lifecycle=resident)\n  subagent: inline (watcher)\n\nTransitions:\n- collect -> review\n- [collect, review] -> watch when verdict.approved eq \"\\u00e9\"\n\n```machine-spec\n{\n  \"run\": {\n    \"failure_policy\": \"continue\",\n    \"max_parallel\": 2,\n    \"budget_ms\": 900000\n  },\n  \"states\": [\n    {\n      \"id\": \"collect\",\n      \"entry\": true,\n      \"subagent\": {\n        \"prompt\": \"Collect the findings for the branch.\"\n      },\n      \"outputs\": [\n        {\n          \"name\": \"findings\",\n          \"type\": \"text\"\n        }\n      ]\n    },\n    {\n      \"id\": \"review\",\n      \"subagent\": \"reviewer\",\n      \"max_entries\": 3,\n      \"inputs\": [\n        {\n          \"name\": \"draft\",\n          \"type\": \"text\",\n          \"from\": \"collect.findings\",\n          \"optional\": true\n        }\n      ],\n      \"foreach\": {\n        \"over\": \"draft\",\n        \"max\": 4\n      }\n    },\n    {\n      \"id\": \"watch\",\n      \"lifecycle\": \"resident\",\n      \"subagent\": {\n        \"name\": \"watcher\",\n        \"prompt\": \"Stay.\"\n      }\n    }\n  ],\n  \"transitions\": [\n    {\n      \"from\": \"collect\",\n      \"to\": \"review\"\n    },\n    {\n      \"from\": [\n        \"collect\",\n        \"review\"\n      ],\n      \"to\": \"watch\",\n      \"when\": {\n        \"output\": \"verdict\",\n        \"path\": \"approved\",\n        \"op\": \"eq\",\n        \"value\": \"é\"\n      }\n    }\n  ]\n}\n```\n";
    assert_eq!(text, expected);
    // The renderer's own failures surface as the original raised them.
    let refusal = render_machine_file(&RenderFields {
        name: &py(&json!("sweep")),
        description: &py(&json!(5)),
        version: &py(&json!("1")),
        author: &py(&json!("")),
        spec: &spec,
        spec_json: Ok(&fence),
    });
    assert_eq!(
        refusal,
        Err(Raise::Type(
            "expected string or bytes-like object, got 'int'".to_string()
        ))
    );
}

/// A two-level library in a temp dir: `(dir, fs at the dir, dirs)`.
fn library() -> (tempfile::TempDir, Fs, LibraryDirs) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let fs = Fs::new(dir.path().to_path_buf());
    let dirs = LibraryDirs(vec![
        ("repo".to_string(), dir.path().join("repo")),
        ("user".to_string(), dir.path().join("user")),
    ]);
    (dir, fs, dirs)
}

fn write_machine(root: &Path, directory: &str, name: &str, spec: &str) -> PathBuf {
    let path = root.join(directory).join(MACHINE_FILE_NAME);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let frontmatter = format!("---\nname: {name}\ndescription: The {directory} machine.\n---");
    std::fs::write(
        &path,
        machine_text(&frontmatter, &format!("```machine-spec\n{spec}\n```\n")),
    )
    .expect("write");
    path
}

#[test]
fn resolution_is_repo_first_and_never_lets_a_broken_file_claim_a_name() {
    let (_dir, fs, dirs) = library();
    let repo = &dirs.0[0].1;
    let user = &dirs.0[1].1;
    // An invalid repo file never shadows the valid user machine.
    write_machine(repo, "sweep", "sweep", "{\"states\": []}");
    let user_sweep = write_machine(user, "sweep", "sweep", SPEC);
    // Repo wins a name both levels carry validly.
    let repo_other = write_machine(repo, "other", "other", SPEC);
    write_machine(user, "other", "other", SPEC);
    // A directory's declared name is the machine's name.
    let renamed = write_machine(user, "folder", "renamed", SPEC);
    std::fs::create_dir_all(user.join("bytes")).expect("dir");
    std::fs::write(user.join("bytes").join(MACHINE_FILE_NAME), b"\xff\xfe").expect("write");

    let (listed, warnings) = scan_machine_library(&fs, &dirs).expect("scan");
    let names: Vec<(&str, &str)> = listed
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap(),
                row["source"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [("other", "repo"), ("renamed", "user"), ("sweep", "user")]
    );
    assert_eq!(
        warnings,
        [
            format!(
                "{}: factory machine must declare between 1 and 1024 states, got 0",
                repo.join("sweep").join(MACHINE_FILE_NAME).display()
            ),
            format!(
                "{}: not valid UTF-8 ('utf-8' codec can't decode byte 0xff in position 0: invalid start byte)",
                user.join("bytes").join(MACHINE_FILE_NAME).display()
            ),
        ]
    );
    let resolved = |name: &str| resolve_machine(&fs, &py(&json!(name)), &dirs);
    assert_eq!(resolved("sweep").map(|(_, path)| path), Ok(user_sweep));
    assert_eq!(resolved("other").map(|(_, path)| path), Ok(repo_other));
    assert_eq!(resolved("renamed").map(|(_, path)| path), Ok(renamed));
    assert_eq!(
        resolved("bytes").map(|(_, path)| path),
        Err(Raise::Resolution {
            message: warnings[1].clone(),
            broken: true
        })
    );
    assert_eq!(
        resolved("ghost").map(|(_, path)| path),
        Err(Raise::Resolution {
            message: "unknown machine 'ghost': no MACHINE.md for it in the machine library (machines: other, renamed, sweep)".to_string(),
            broken: false
        })
    );
    assert_eq!(
        resolved("Bad Name").map(|(_, path)| path),
        Err(Raise::Value(
            "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string()
        ))
    );
}

#[test]
fn import_persists_valid_files_byte_for_byte_and_refuses_the_rest() {
    let (dir, fs, dirs) = library();
    let user = dirs.0[1].1.clone();
    let source = dir.path().join("source.MACHINE.md");
    let text =
        machine_text(FRONTMATTER, &format!("```machine-spec\n{SPEC}\n```")).replace('\n', "\r\n");
    std::fs::write(&source, &text).expect("write");
    let imported = import_machine(&fs, &source, &user).expect("imports");
    let stored = user.join("sweep").join(MACHINE_FILE_NAME);
    assert_eq!(
        imported,
        json!({"name": "sweep", "path": stored.display().to_string(), "created": true})
    );
    assert_eq!(std::fs::read(&stored).expect("stored"), text.as_bytes());
    assert_eq!(
        import_machine(&fs, &source, &user).expect("re-imports")["created"],
        json!(false)
    );
    std::fs::write(
        &source,
        machine_text(FRONTMATTER, "```machine-spec\n{\"run\": {\"max_parallel\": null}, \"states\": [{\"id\": \"a\", \"entry\": true, \"subagent\": {\"prompt\": \"P.\"}}]}\n```"),
    )
    .expect("write");
    assert_eq!(
        import_machine(&fs, &source, &dir.path().join("elsewhere")),
        Err(Raise::Value(
            "run max_parallel must be an integer between 1 and 64".to_string()
        ))
    );
    assert!(!dir.path().join("elsewhere").exists());
    assert_eq!(
        import_machine(&fs, &dir.path().join("missing.md"), &user),
        Err(Raise::Value(format!(
            "machine file not found: {}",
            dir.path().join("missing.md").display()
        )))
    );
}

#[cfg(unix)]
#[test]
fn exports_never_clobber_a_target_and_follow_no_planted_symlink() {
    let (dir, fs, dirs) = library();
    write_machine(&dirs.0[0].1, "sweep", "sweep", SPEC);
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "keep me").expect("victim");
    let link = dir.path().join("link.MACHINE.md");
    std::os::unix::fs::symlink(&victim, &link).expect("symlink");
    let name = py(&json!("sweep"));
    assert_eq!(
        export_library_machine(&fs, &name, &link, &dirs, false),
        Err(Raise::Value(format!(
            "export path {} already exists (pass overwrite=True to replace it)",
            link.display()
        )))
    );
    assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep me");
    let out = dir
        .path()
        .join("out")
        .join("nested")
        .join("sweep.MACHINE.md");
    assert_eq!(
        export_library_machine(&fs, &name, &out, &dirs, false),
        Ok(json!({"name": "sweep", "path": out.display().to_string(), "source": "library"}))
    );
    assert_eq!(
        export_library_machine(&fs, &name, &dir.path().join("out"), &dirs, true),
        Err(Raise::Value(format!(
            "export path {} is a directory (pass a file path)",
            dir.path().join("out").display()
        )))
    );
}

#[test]
fn relative_paths_are_the_kernels_not_the_hosts() {
    // The host process runs elsewhere: a relative path the kernel names
    // resolves against the kernel's working directory, and every result
    // and error spells it as the kernel named it.
    let (dir, fs, _) = library();
    let dirs = LibraryDirs(vec![
        ("repo".to_string(), PathBuf::from("lib")),
        ("user".to_string(), PathBuf::from("mine")),
    ]);
    write_machine(&dir.path().join("lib"), "sweep", "sweep", SPEC);
    let (listed, _) = scan_machine_library(&fs, &dirs).expect("scan");
    assert_eq!(listed[0]["path"], json!("lib/sweep/MACHINE.md"));
    let out = PathBuf::from("exports").join("sweep.MACHINE.md");
    assert_eq!(
        export_library_machine(&fs, &py(&json!("sweep")), &out, &dirs, false),
        Ok(json!({"name": "sweep", "path": "exports/sweep.MACHINE.md", "source": "library"}))
    );
    assert!(
        dir.path()
            .join("exports")
            .join("sweep.MACHINE.md")
            .is_file()
    );
}

#[test]
fn an_entry_or_run_exports_byte_pretty_and_names_what_cannot_become_a_machine() {
    let (dir, fs, dirs) = library();
    let arguments = py(
        &json!({"machine": {"states": [{"id": "a", "entry": true, "subagent": {"prompt": "P."}}]}}),
    );
    let entry = ExportSource::Entry {
        arguments: arguments.clone(),
        content: py(&json!("First line.\nSecond\tline.")),
        title: py(&json!("Title")),
    };
    let out = dir.path().join("entry.MACHINE.md");
    assert_eq!(
        export_machine(&fs, &py(&json!("sweep")), &entry, &out, &dirs, false),
        Ok(json!({"name": "sweep", "path": out.display().to_string(), "source": "spec"}))
    );
    let rendered = std::fs::read_to_string(&out).expect("rendered");
    assert!(
        rendered.contains("description: First line. Second line.\n"),
        "{rendered}"
    );
    assert_eq!(
        export_machine(&fs, &py(&json!("Sweep Entry")), &entry, &out, &dirs, true),
        Err(Raise::Value(
            "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only); \
             the stored entry id 'Sweep Entry' cannot become a machine name"
                .to_string()
        ))
    );
    let empty = ExportSource::Entry {
        arguments: py(&json!({})),
        content: py(&json!("")),
        title: py(&json!("")),
    };
    assert_eq!(
        export_machine(&fs, &py(&json!("sweep")), &empty, &out, &dirs, true),
        Err(Raise::Value(
            "factory entry 'sweep' carries no machine or dag spec".to_string()
        ))
    );
    let run = ExportSource::Run(py(&json!({
        "run_id": "run-1",
        "spec_id": "sweep",
        "name": null,
        "machine": arguments.get("machine").to_json(),
    })));
    let run_out = dir.path().join("run.MACHINE.md");
    assert_eq!(
        export_machine(&fs, &py(&json!("run-1")), &run, &run_out, &dirs, false)
            .map(|result| result["source"].clone()),
        Ok(json!("spec"))
    );
    assert!(
        std::fs::read_to_string(&run_out)
            .expect("run export")
            .contains("description: factory run run-1\n")
    );
}

#[test]
fn the_cli_facade_reports_every_failure_as_data() {
    let (_dir, fs, dirs) = library();
    for (payload, errors) in [
        (
            json!([]),
            json!(["factory cli payload must be a JSON object"]),
        ),
        (
            json!({"op": "wat"}),
            json!(["unknown factory cli op 'wat' (expected 'list', 'import' or 'export')"]),
        ),
        (
            json!({"op": "import", "path": ""}),
            json!(["factory import requires a `path` string"]),
        ),
        (
            json!({"op": "export"}),
            json!(["factory export requires a `name` string"]),
        ),
        (
            json!({"op": "export", "name": "x"}),
            json!(["factory export requires an `out` string"]),
        ),
    ] {
        assert_eq!(
            cli_dispatch(&fs, &py(&payload), &dirs),
            Ok(json!({"ok": false, "errors": errors})),
            "{payload}"
        );
    }
    assert_eq!(
        cli_dispatch(&fs, &py(&json!({"op": "list"})), &dirs),
        Ok(json!({"ok": true, "machines": [], "warnings": []}))
    );
}

#[test]
fn the_request_envelope_carries_the_exception_to_raise() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let request = |op: &str, extra: Value| {
        let mut data = json!({"op": op, "cwd": dir.path().display().to_string()});
        if let (Some(data), Some(extra)) = (data.as_object_mut(), extra.as_object()) {
            data.extend(extra.clone());
        }
        handle_request(&data).expect("well formed")
    };
    let table = |value: Value| encode_node_table(&py(&value));
    assert_eq!(
        request("name_errors", json!({"value": table(json!("Bad"))})),
        json!({"ok": true, "result": ["machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"]})
    );
    std::fs::create_dir(dir.path().join("dir.md")).expect("dir");
    let refused = request("import", json!({"path": "dir.md", "target_dir": "target"}));
    assert_eq!(
        refused,
        json!({"ok": false, "error": {"type": "ValueError", "message": "machine file not found: dir.md"}})
    );
    std::fs::write(dir.path().join("bytes.md"), b"ok\xe2\x82").expect("write");
    assert_eq!(
        request(
            "import",
            json!({"path": "bytes.md", "target_dir": "target"})
        ),
        json!({"ok": false, "error": {
            "type": "UnicodeDecodeError",
            "message": "'utf-8' codec can't decode bytes in position 2-3: unexpected end of data",
            "start": 2, "end": 4, "reason": "unexpected end of data", "bytes": [0xe2, 0x82],
        }})
    );
    assert!(handle_request(&json!({"op": "nope"})).is_err());
}
