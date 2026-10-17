use std::fs;
use std::path::{Path, PathBuf};

use super::*;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let agent_dir = home.join(".prime").join("agent");
        let repo = home.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
            agent_dir,
            repo,
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn options(&self, cwd: &Path) -> InstructionOptions {
        InstructionOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: self.agent_dir.clone(),
            home_dir: Some(self.home.clone()),
            claude_user_files: false,
            additional_dirs: Vec::new(),
        }
    }

    fn load(&self, cwd: &Path) -> (Vec<InstructionFile>, NestedInstructions) {
        load_instructions(self.options(cwd))
    }

    /// Paths relative to the fixture root, in load order.
    fn names(&self, files: &[InstructionFile]) -> Vec<String> {
        files
            .iter()
            .map(|file| {
                file.path
                    .strip_prefix(&self.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }
}

#[test]
fn discovery_order_is_user_then_root_to_cwd_with_every_family_name() {
    let fx = Fixture::new();
    Fixture::write(&fx.agent_dir.join("AGENTS.md"), "global");
    Fixture::write(&fx.agent_dir.join("rules").join("style.md"), "user rule");
    Fixture::write(&fx.home.join("AGENTS.md"), "home agents");
    Fixture::write(&fx.repo.join("AGENTS.md"), "repo agents");
    Fixture::write(&fx.repo.join("CLAUDE.md"), "repo claude");
    Fixture::write(&fx.repo.join(".prime/agent/AGENTS.md"), "repo prime");
    Fixture::write(&fx.repo.join(".claude/CLAUDE.md"), "repo dot claude");
    Fixture::write(&fx.repo.join(".prime/agent/rules/a.md"), "prime rule");
    Fixture::write(&fx.repo.join(".claude/rules/b.md"), "claude rule");
    Fixture::write(&fx.repo.join("AGENTS.local.md"), "agents local");
    Fixture::write(&fx.repo.join("CLAUDE.local.md"), "claude local");
    let (files, _) = fx.load(&fx.repo);
    assert_eq!(
        fx.names(&files),
        vec![
            "home/.prime/agent/AGENTS.md",
            "home/.prime/agent/rules/style.md",
            "home/AGENTS.md",
            "home/repo/AGENTS.md",
            "home/repo/CLAUDE.md",
            "home/repo/.prime/agent/AGENTS.md",
            "home/repo/.claude/CLAUDE.md",
            "home/repo/.prime/agent/rules/a.md",
            "home/repo/.claude/rules/b.md",
            "home/repo/AGENTS.local.md",
            "home/repo/CLAUDE.local.md",
        ]
    );
    let kinds: Vec<InstructionKind> = files.iter().map(|f| f.kind).collect();
    assert_eq!(kinds[0], InstructionKind::User);
    assert_eq!(kinds[1], InstructionKind::User);
    assert_eq!(kinds[3], InstructionKind::Project);
    assert_eq!(kinds[9], InstructionKind::Local);
}

#[test]
fn a_copied_or_linked_file_loads_once() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("AGENTS.md"), "same rules\n");
    Fixture::write(&fx.repo.join("CLAUDE.md"), "same rules");
    let (files, _) = fx.load(&fx.repo);
    assert_eq!(fx.names(&files), vec!["home/repo/AGENTS.md"]);

    #[cfg(unix)]
    {
        let fx = Fixture::new();
        Fixture::write(&fx.repo.join("AGENTS.md"), "linked");
        std::os::unix::fs::symlink(fx.repo.join("AGENTS.md"), fx.repo.join("CLAUDE.md")).unwrap();
        let (files, _) = fx.load(&fx.repo);
        assert_eq!(fx.names(&files), vec!["home/repo/AGENTS.md"]);
    }
}

#[test]
fn home_dot_claude_is_user_scope_and_opt_in() {
    let fx = Fixture::new();
    Fixture::write(&fx.home.join(".claude/CLAUDE.md"), "cc user");
    Fixture::write(&fx.home.join(".claude/rules/r.md"), "cc user rule");
    let (files, _) = fx.load(&fx.repo);
    assert!(files.is_empty(), "{:?}", fx.names(&files));

    let mut options = fx.options(&fx.repo);
    options.claude_user_files = true;
    let (files, _) = load_instructions(options);
    assert_eq!(
        fx.names(&files),
        vec!["home/.claude/CLAUDE.md", "home/.claude/rules/r.md"]
    );
    assert!(files.iter().all(|f| f.kind == InstructionKind::User));
}

#[test]
fn imports_load_after_their_file_recursively_once() {
    let fx = Fixture::new();
    Fixture::write(
        &fx.repo.join("CLAUDE.md"),
        "Read @AGENTS.md and @docs/style.md.\n```\n@docs/fenced.md\n```\nInline `@docs/inline.md`.",
    );
    Fixture::write(&fx.repo.join("AGENTS.md"), "agents body @docs/style.md");
    Fixture::write(&fx.repo.join("docs/style.md"), "style @../CLAUDE.md");
    Fixture::write(&fx.repo.join("docs/fenced.md"), "fenced");
    Fixture::write(&fx.repo.join("docs/inline.md"), "inline");
    let (files, _) = fx.load(&fx.repo);
    // AGENTS.md loads first in its directory; its import follows it; the
    // CLAUDE.md import of AGENTS.md and the cycle back to CLAUDE.md are no-ops.
    assert_eq!(
        fx.names(&files),
        vec![
            "home/repo/AGENTS.md",
            "home/repo/docs/style.md",
            "home/repo/CLAUDE.md"
        ]
    );
    assert_eq!(
        files[1].parent.as_deref(),
        Some(fx.repo.join("AGENTS.md").as_path())
    );
    assert_eq!(files[0].parent, None);
}

#[test]
fn imports_stop_at_the_depth_cap() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("AGENTS.md"), "@d1.md");
    for level in 1..=7 {
        Fixture::write(
            &fx.repo.join(format!("d{level}.md")),
            &format!("level {level} @d{}.md", level + 1),
        );
    }
    let (files, _) = fx.load(&fx.repo);
    // The top file plus MAX_IMPORT_DEPTH hops.
    assert_eq!(files.len(), 1 + imports::MAX_IMPORT_DEPTH);
    assert_eq!(
        files.last().unwrap().path,
        fx.repo.join(format!("d{}.md", imports::MAX_IMPORT_DEPTH))
    );
}

#[test]
fn project_imports_stay_inside_their_directory_but_user_imports_do_not() {
    let fx = Fixture::new();
    Fixture::write(&fx.home.join("secret.md"), "SECRET");
    Fixture::write(&fx.home.join("notes.md"), "user notes");
    Fixture::write(
        &fx.repo.join("AGENTS.md"),
        "@../secret.md @~/secret.md @inside.md",
    );
    Fixture::write(&fx.repo.join("inside.md"), "inside");
    Fixture::write(&fx.agent_dir.join("AGENTS.md"), "global @~/notes.md");
    let (files, _) = fx.load(&fx.repo);
    assert_eq!(
        fx.names(&files),
        vec![
            "home/.prime/agent/AGENTS.md",
            "home/notes.md",
            "home/repo/AGENTS.md",
            "home/repo/inside.md"
        ]
    );
    assert!(files.iter().all(|f| f.content != "SECRET"));
}

#[test]
fn conditional_rules_wait_for_a_matching_path() {
    let fx = Fixture::new();
    Fixture::write(
        &fx.repo.join(".claude/rules/rust.md"),
        "---\npaths:\n  - \"crates/**/*.rs\"\n---\nRust rule @rust-import.md",
    );
    Fixture::write(
        &fx.repo.join(".claude/rules/rust-extra.md"),
        "---\npaths: none/**\n---\nunused",
    );
    Fixture::write(&fx.repo.join(".claude/rules/rust-import.md"), "x");
    Fixture::write(
        &fx.repo.join(".prime/agent/rules/py.md"),
        "---\npaths: \"*.py\"\n---\nPython rule",
    );
    Fixture::write(&fx.repo.join(".claude/rules/always.md"), "always");
    let (files, nested) = fx.load(&fx.repo);
    // `rust-import.md` and `always.md` are unconditional.
    assert_eq!(
        fx.names(&files),
        vec![
            "home/repo/.claude/rules/always.md",
            "home/repo/.claude/rules/rust-import.md"
        ]
    );
    assert_eq!(nested.pending_rules().len(), 3);

    assert!(nested.touch(&[fx.repo.join("README.md")]).is_empty());
    let delivered = nested.touch(&[fx.repo.join("crates/a/src/lib.rs")]);
    assert_eq!(
        fx.names(&delivered),
        vec!["home/repo/.claude/rules/rust.md"]
    );
    assert_eq!(delivered[0].content, "Rust rule @rust-import.md");
    assert_eq!(delivered[0].globs, vec!["crates/**/*.rs"]);
    // Delivered once.
    assert!(nested.touch(&[fx.repo.join("crates/b/x.rs")]).is_empty());

    let delivered = nested.touch(&[Path::new("tools/gen.py")]);
    assert_eq!(
        fx.names(&delivered),
        vec!["home/repo/.prime/agent/rules/py.md"]
    );
}

#[test]
fn nested_directories_deliver_their_files_on_first_touch() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("AGENTS.md"), "top");
    Fixture::write(&fx.repo.join("crates/AGENTS.md"), "crates level");
    Fixture::write(
        &fx.repo.join("crates/core/CLAUDE.md"),
        "core level @notes.md",
    );
    Fixture::write(&fx.repo.join("crates/core/notes.md"), "core notes");
    Fixture::write(&fx.repo.join("crates/core/CLAUDE.local.md"), "core local");
    Fixture::write(
        &fx.repo.join("crates/core/.claude/rules/tests.md"),
        "---\npaths: \"tests/**\"\n---\ncore tests rule",
    );
    Fixture::write(&fx.repo.join("crates/core/src/lib.rs"), "");
    Fixture::write(&fx.repo.join("other/AGENTS.md"), "other");
    let (files, nested) = fx.load(&fx.repo);
    assert_eq!(fx.names(&files), vec!["home/repo/AGENTS.md"]);

    let delivered = nested.touch(&[fx.repo.join("crates/core/src/lib.rs")]);
    assert_eq!(
        fx.names(&delivered),
        vec![
            "home/repo/crates/AGENTS.md",
            "home/repo/crates/core/CLAUDE.md",
            "home/repo/crates/core/notes.md",
            "home/repo/crates/core/CLAUDE.local.md",
        ]
    );
    assert_eq!(delivered[3].kind, InstructionKind::Local);
    // Each directory once; the nested directory's own conditional rule
    // matches relative to that directory.
    assert!(
        nested
            .touch(&[fx.repo.join("crates/core/Cargo.toml")])
            .is_empty()
    );
    let delivered = nested.touch(&[fx.repo.join("crates/core/tests/it.rs")]);
    assert_eq!(
        fx.names(&delivered),
        vec!["home/repo/crates/core/.claude/rules/tests.md"]
    );
    // Paths outside the working directory deliver nothing nested.
    assert!(
        nested
            .touch(&[fx.root.join("elsewhere/AGENTS.md")])
            .is_empty()
    );

    let rendered = render_delivered(&nested.touch(&[fx.repo.join("other/x.txt")]));
    assert_eq!(
        rendered,
        format!(
            "Contents of {} (project instructions, checked into the codebase):\n\nother",
            fx.repo.join("other/AGENTS.md").display()
        )
    );
}

#[test]
fn delivered_files_from_an_earlier_run_are_not_delivered_again() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("sub/AGENTS.md"), "sub");
    Fixture::write(&fx.repo.join("sub/CLAUDE.md"), "sub claude");
    Fixture::write(
        &fx.repo.join(".claude/rules/md.md"),
        "---\npaths: \"*.md\"\n---\nmd rule",
    );
    let (_, nested) = fx.load(&fx.repo);
    nested.mark_delivered(&[
        fx.repo.join("sub/AGENTS.md"),
        fx.repo.join(".claude/rules/md.md"),
    ]);
    assert!(nested.pending_rules().is_empty());
    let delivered = nested.touch(&[fx.repo.join("sub/x.md")]);
    assert_eq!(fx.names(&delivered), vec!["home/repo/sub/CLAUDE.md"]);
}

#[test]
fn binary_and_empty_files_are_skipped_and_large_files_cut() {
    let fx = Fixture::new();
    fs::write(fx.repo.join("AGENTS.md"), b"\x00\x01binary").unwrap();
    Fixture::write(&fx.repo.join("CLAUDE.md"), "   \n");
    let (files, _) = fx.load(&fx.repo);
    assert!(files.is_empty());

    let fx = Fixture::new();
    Fixture::write(
        &fx.repo.join("AGENTS.md"),
        &"x".repeat(MAX_INSTRUCTION_FILE_BYTES + 10),
    );
    let (files, _) = fx.load(&fx.repo);
    assert!(files[0].content.ends_with("bytes]"));
    assert!(files[0].content.len() < MAX_INSTRUCTION_FILE_BYTES + 100);
}

#[test]
fn files_the_conversation_lost_are_delivered_again() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("sub/AGENTS.md"), "sub");
    Fixture::write(&fx.repo.join("sub/CLAUDE.md"), "sub claude");
    Fixture::write(
        &fx.repo.join(".claude/rules/md.md"),
        "---\npaths: \"*.md\"\n---\nmd rule",
    );
    let (_, nested) = fx.load(&fx.repo);
    let first = nested.touch(&[fx.repo.join("sub/x.md")]);
    assert_eq!(
        fx.names(&first),
        vec![
            "home/repo/sub/AGENTS.md",
            "home/repo/sub/CLAUDE.md",
            "home/repo/.claude/rules/md.md"
        ]
    );
    assert!(nested.pending_rules().is_empty());
    // Every row still in the conversation: nothing changes.
    nested.forget_missing(&nested.delivered());
    assert!(nested.touch(&[fx.repo.join("sub/x.md")]).is_empty());
    // A compaction kept only the CLAUDE.md row.
    nested.forget_missing(&[fx.repo.join("sub/CLAUDE.md")]);
    assert_eq!(nested.pending_rules().len(), 1);
    let again = nested.touch(&[fx.repo.join("sub/y.md")]);
    assert_eq!(
        fx.names(&again),
        vec!["home/repo/sub/AGENTS.md", "home/repo/.claude/rules/md.md"]
    );
    assert_eq!(nested.delivered().len(), 3);
}

#[test]
fn additional_directories_load_their_own_files_after_the_working_directory() {
    let fx = Fixture::new();
    Fixture::write(&fx.repo.join("AGENTS.md"), "repo agents");
    let extra = fx.root.join("extra");
    Fixture::write(&extra.join("CLAUDE.md"), "extra claude");
    Fixture::write(&extra.join(".claude/rules/r.md"), "extra rule");
    let mut options = fx.options(&fx.repo);
    options.additional_dirs = vec![extra, fx.repo.clone()];
    let (files, _) = load_instructions(options);
    assert_eq!(
        fx.names(&files),
        vec![
            "home/repo/AGENTS.md",
            "extra/CLAUDE.md",
            "extra/.claude/rules/r.md",
        ]
    );
}
