use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

/// Tree A: a runtime module and a skill module as a build embeds them.
const TREE_A: &[BundleFile] = &[
    (
        "prime-agent-runtime/pyproject.toml",
        b"[project]\nname = \"prime-agent-runtime\"\n",
    ),
    (
        "prime-agent-runtime/src/rlm/bash.py",
        b"SERVED_BY = 'host'\n",
    ),
    (
        "skills/computer-use/SKILL.md",
        b"---\nname: computer-use\n---\n",
    ),
    (
        "skills/computer-use/src/computer_use/__init__.py",
        b"CLIENT = 'thin'\n",
    ),
];

/// Tree B: the checkout after tree A was built, a runtime and a skill edit later.
const TREE_B: &[BundleFile] = &[
    (
        "prime-agent-runtime/pyproject.toml",
        b"[project]\nname = \"prime-agent-runtime\"\n",
    ),
    (
        "prime-agent-runtime/src/rlm/bash.py",
        b"SERVED_BY = 'sidecar'\n",
    ),
    (
        "skills/computer-use/SKILL.md",
        b"---\nname: computer-use\n---\n",
    ),
    (
        "skills/computer-use/src/computer_use/__init__.py",
        b"CLIENT = 'fat'\n",
    ),
];

fn read(dir: &Path, relative: &str) -> String {
    std::fs::read_to_string(bundle_file_path(dir, relative)).unwrap()
}

fn entry_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// The build embeds the runtime and the bundled skills (feature skills
/// included) under the packer's content policy: no caches, no runtime tests
/// or lockfile.
#[test]
fn the_build_embeds_the_runtime_and_skills_without_dev_files() {
    let paths: Vec<&str> = EMBEDDED_FILES.iter().map(|(path, _)| *path).collect();
    for required in [
        "prime-agent-runtime/pyproject.toml",
        "prime-agent-runtime/kernel-constraints.txt",
        "prime-agent-runtime/src/rlm/repl.py",
        "prime-agent-runtime/src/rlm/bash.py",
        "prime-agent-runtime/schemas/workflow-v2.schema.json",
        "skills/computer-use/SKILL.md",
        "skills/computer-use/pyproject.toml",
        "skills/.features/dream/SKILL.md",
    ] {
        assert!(paths.contains(&required), "{required} is not embedded");
    }
    let shipped_dev_files: Vec<&&str> = paths
        .iter()
        .filter(|path| {
            path.contains("__pycache__")
                || Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pyc"))
                || path.contains("/.venv/")
                || path.starts_with("prime-agent-runtime/test/")
                || **path == "prime-agent-runtime/uv.lock"
        })
        .collect();
    assert_eq!(shipped_dev_files, Vec::<&&str>::new());
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "the index is sorted");
    // Each embedded file is the build tree's current content.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repl = EMBEDDED_FILES
        .iter()
        .find(|(path, _)| *path == "prime-agent-runtime/src/rlm/repl.py")
        .unwrap();
    assert_eq!(
        repl.1,
        std::fs::read(root.join("prime-agent-runtime/src/rlm/repl.py")).unwrap()
    );
}

/// The installed binary built from tree A keeps serving tree A after its
/// checkout moves on to tree B: the bundle is a content-addressed copy of
/// what was built, and a build of tree B lands beside it, not over it.
#[test]
fn a_bundle_keeps_the_built_tree_after_the_checkout_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("runtime");
    let a = materialize(&root, TREE_A).unwrap();
    assert_eq!(a, root.join(bundle_id(TREE_A)));
    let b = materialize(&root, TREE_B).unwrap();
    assert_ne!(a, b);
    assert_eq!(
        read(&a, "prime-agent-runtime/src/rlm/bash.py"),
        "SERVED_BY = 'host'\n"
    );
    assert_eq!(
        read(&a, "skills/computer-use/src/computer_use/__init__.py"),
        "CLIENT = 'thin'\n"
    );
    assert_eq!(
        read(&b, "prime-agent-runtime/src/rlm/bash.py"),
        "SERVED_BY = 'sidecar'\n"
    );
    // Tree A again (a downgrade): the same directory, untouched.
    assert_eq!(materialize(&root, TREE_A).unwrap(), a);
}

/// Extraction is idempotent, leaves no scratch behind, and repairs a
/// damaged bundle instead of serving it.
#[test]
fn extraction_is_idempotent_and_repairs_damage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("runtime");
    let first = materialize(&root, TREE_A).unwrap();
    let second = materialize(&root, TREE_A).unwrap();
    assert_eq!(first, second);
    assert_eq!(entry_names(&root), vec![bundle_id(TREE_A)]);

    let edited = bundle_file_path(&first, "prime-agent-runtime/src/rlm/bash.py");
    std::fs::write(&edited, "SERVED_BY = 'tampered'\n").unwrap();
    std::fs::remove_file(bundle_file_path(&first, "skills/computer-use/SKILL.md")).unwrap();
    assert_eq!(materialize(&root, TREE_A).unwrap(), first);
    assert!(bundle_matches(&first, TREE_A));
    assert_eq!(entry_names(&root), vec![bundle_id(TREE_A)]);
}

/// Concurrent first boots converge on one complete bundle.
#[test]
fn concurrent_first_boots_converge_on_one_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("runtime");
    let barrier = std::sync::Barrier::new(8);
    let results: Vec<std::io::Result<PathBuf>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    materialize(&root, TREE_A)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let expected = root.join(bundle_id(TREE_A));
    for result in results {
        assert_eq!(result.unwrap(), expected);
    }
    assert!(bundle_matches(&expected, TREE_A));
    assert_eq!(entry_names(&root), vec![bundle_id(TREE_A)]);
}

fn fake_bundle(root: &Path, id: &str, used: SystemTime) {
    let dir = root.join(id);
    std::fs::create_dir_all(dir.join("skills/goal")).unwrap();
    let marker = std::fs::File::create(dir.join(LAST_USED_FILE)).unwrap();
    marker.set_modified(used).unwrap();
}

fn fake_scratch(root: &Path, name: &str, modified: SystemTime) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::open(&dir)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

/// Pruning removes only old bundles nothing needs: never the current one,
/// the few most recently used, one used this week (another binary may be
/// running it), or one a kernel venv's editable skill install points into.
#[test]
fn pruning_keeps_every_bundle_that_may_be_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("runtime");
    let now = SystemTime::now();
    let days = |n: u64| now - Duration::from_hours(n * 24);
    let id = |n: u8| format!("{n:0>20}");
    fake_bundle(&root, &id(0), days(90)); // current, however old
    fake_bundle(&root, &id(1), days(10)); // newest three others
    fake_bundle(&root, &id(2), days(20));
    fake_bundle(&root, &id(3), days(30));
    fake_bundle(&root, &id(4), days(40)); // protected by the venv
    fake_bundle(&root, &id(5), days(50)); // removed
    fake_bundle(&root, &id(6), days(60)); // removed
    std::fs::create_dir_all(root.join("not-a-bundle")).unwrap();
    fake_scratch(&root, ".tmp-abandoned", days(2));
    fake_scratch(&root, ".tmp-in-flight", now);
    let protected = vec![root.join(id(4)).join("skills").join("goal")];

    let removed = prune(&root, &id(0), &protected, now);
    assert_eq!(removed, vec![id(5), id(6)]);
    let mut expected = vec![
        ".tmp-in-flight".to_string(),
        id(0),
        id(1),
        id(2),
        id(3),
        id(4),
        "not-a-bundle".to_string(),
    ];
    expected.sort();
    assert_eq!(entry_names(&root), expected);

    // A bundle used within the week is kept even past the newest three.
    fake_bundle(&root, &id(7), days(1));
    fake_bundle(&root, &id(8), days(2));
    fake_bundle(&root, &id(9), days(3));
    fake_bundle(&root, &id(1), days(6));
    assert_eq!(prune(&root, &id(0), &protected, now), vec![id(2), id(3)]);
}
