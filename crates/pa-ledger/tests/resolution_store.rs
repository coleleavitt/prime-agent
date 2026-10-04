//! The durable resolution store (TS `resolution-index-durable.test.ts`).

use std::path::Path;

use pa_ledger::{
    find_repo_dir, open_resolution_store, resolution_dir, resolution_store_path, ResolutionCell,
    ResolutionIndex, ResolutionIndexOptions, ResolutionOrigin, ResolutionStore,
    DEFAULT_MAX_RESOLUTIONS,
};

const FAILING_CELL: &str = "agents = client.list_agents()";
const FIX_CELL: &str = "agents = client.agents()\nprint(f\"{len(agents)} agents\")";
const RECURRENCE_CELL: &str = "for agent in client.list_agents():\n    print(agent.name)";

fn attribute_error(line: u32, source: &str) -> String {
    format!(
        "Traceback (most recent call last):\n  File \"<ipython-input-{line}>\", line 1, in <module>\n    {source}\nAttributeError: 'AgentClient' object has no attribute 'list_agents'"
    )
}

fn git_repo(root: &Path, name: &str) -> std::path::PathBuf {
    let repo = root.join(name);
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    repo
}

fn index(repo: &Path, agent: &Path, max_records: Option<usize>) -> ResolutionIndex {
    ResolutionIndex::new(ResolutionIndexOptions {
        store: open_resolution_store(repo, agent)
            .map(|store| Box::new(store) as Box<dyn ResolutionStore>),
        max_records: max_records.unwrap_or(DEFAULT_MAX_RESOLUTIONS),
        ..ResolutionIndexOptions::default()
    })
}

fn cell<'a>(code: &'a str, output: &'a str, is_error: bool) -> ResolutionCell<'a> {
    ResolutionCell {
        code,
        output,
        is_error,
    }
}

fn learn_the_fix(repo: &Path, agent: &Path) -> ResolutionIndex {
    let mut session = index(repo, agent, None);
    session.observe(cell(FAILING_CELL, &attribute_error(1, FAILING_CELL), true));
    session.observe(cell(FIX_CELL, "3 agents", false));
    session
}

#[test]
fn a_fix_recorded_by_one_session_reaches_a_later_session_in_the_same_repo() {
    let root = tempfile::tempdir().unwrap();
    let (repo, agent) = (git_repo(root.path(), "repo"), root.path().join("agent"));
    assert_eq!(learn_the_fix(&repo, &agent).records().len(), 1);

    let mut later = index(&repo.join("nested"), &agent, None);
    assert_eq!(later.records(), Vec::new());
    let hint = later
        .observe(cell(
            RECURRENCE_CELL,
            &attribute_error(3, "for agent in client.list_agents():"),
            true,
        ))
        .unwrap();
    assert_eq!(hint.origin, ResolutionOrigin::Store);
    assert_eq!(hint.record.fix, FIX_CELL);
    assert!(hint
        .text
        .ends_with("of an earlier session)\n</ipython_resolution_hint>"));
}

#[test]
fn each_repo_keeps_its_resolutions_to_itself() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    learn_the_fix(&git_repo(root.path(), "repo"), &agent);
    let mut elsewhere = index(&git_repo(root.path(), "other"), &agent, None);
    let hint = elsewhere.observe(cell(
        RECURRENCE_CELL,
        &attribute_error(3, "for agent in client.list_agents():"),
        true,
    ));
    assert_eq!(hint, None);
}

#[test]
fn a_second_session_merges_its_record_instead_of_clobbering_the_first() {
    let root = tempfile::tempdir().unwrap();
    let (repo, agent) = (git_repo(root.path(), "repo"), root.path().join("agent"));
    learn_the_fix(&repo, &agent);
    let mut second = index(&repo, &agent, None);
    let failure = "Traceback (most recent call last):\n  File \"<ipython-input-1>\", line 1, in <module>\n    total = frame.agg(how)\nValueError: cannot reindex on an axis with duplicate labels";
    second.observe(cell("total = frame.agg(how)", failure, true));
    second.observe(cell("total = frame.agg('sum')", "ok", false));
    let mut fixes: Vec<String> = second
        .durable_records()
        .into_iter()
        .map(|record| record.fix)
        .collect();
    fixes.sort();
    assert_eq!(
        fixes,
        vec![FIX_CELL.to_string(), "total = frame.agg('sum')".to_string()]
    );
}

#[test]
fn a_corrupt_store_reads_as_empty_and_never_fails_the_cell() {
    let root = tempfile::tempdir().unwrap();
    let (repo, agent) = (git_repo(root.path(), "repo"), root.path().join("agent"));
    learn_the_fix(&repo, &agent);
    let path = resolution_store_path(&repo.to_string_lossy(), &agent);
    std::fs::write(&path, "{ not json at all").unwrap();
    let mut session = index(&repo, &agent, None);
    let hint = session.observe(cell(
        RECURRENCE_CELL,
        &attribute_error(3, "for agent in client.list_agents():"),
        true,
    ));
    assert_eq!(hint, None);
    std::fs::write(&path, "{\"version\": 2, \"records\": []}").unwrap();
    assert_eq!(session.durable_records(), Vec::new());
}

#[cfg(unix)]
#[test]
fn the_store_is_owner_only_because_a_record_holds_verbatim_cell_source() {
    let root = tempfile::tempdir().unwrap();
    let (repo, agent) = (git_repo(root.path(), "repo"), root.path().join("agent"));
    learn_the_fix(&repo, &agent);
    let path = resolution_store_path(&repo.to_string_lossy(), &agent);
    assert_eq!(
        pa_core::platform::file_mode(&path).map(|mode| mode & 0o777),
        Some(0o600)
    );
    assert_eq!(
        pa_core::platform::file_mode(&resolution_dir(&agent)).map(|mode| mode & 0o777),
        Some(0o700)
    );
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("client.agents()"));
}

#[test]
fn the_store_is_bounded_at_the_retained_record_cap() {
    let root = tempfile::tempdir().unwrap();
    let (repo, agent) = (git_repo(root.path(), "repo"), root.path().join("agent"));
    let mut session = index(&repo, &agent, Some(1));
    for i in 0..DEFAULT_MAX_RESOLUTIONS + 6 {
        let name = "z".repeat(i + 1);
        let failure = format!(
            "Traceback (most recent call last):\n  File \"<ipython-input-1>\", line 1, in <module>\n    handle.{name}()\nAttributeError: module tool has no attribute {name}"
        );
        session.observe(cell(&format!("handle.{name}()"), &failure, true));
        session.observe(cell(&format!("handle.run_{name}()"), "ok", false));
    }
    let durable = session.durable_records();
    assert_eq!(durable.len(), DEFAULT_MAX_RESOLUTIONS);
    assert_eq!(
        durable.last().map(|record| record.fix.clone()),
        Some(format!(
            "handle.run_{}()",
            "z".repeat(DEFAULT_MAX_RESOLUTIONS + 6)
        ))
    );
}

#[test]
fn outside_a_git_repository_the_index_is_session_only() {
    let root = tempfile::tempdir().unwrap();
    let loose = root.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    let agent = root.path().join("agent");
    assert_eq!(find_repo_dir(&loose), None);
    assert!(open_resolution_store(&loose, &agent).is_none());
    let session = learn_the_fix(&loose, &agent);
    assert_eq!(session.records().len(), 1);
    assert_eq!(session.durable_records(), Vec::new());
    assert!(!resolution_dir(&agent).exists());
}

#[test]
fn a_git_dir_without_head_ends_the_walk() {
    let root = tempfile::tempdir().unwrap();
    let outer = git_repo(root.path(), "outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    assert_eq!(find_repo_dir(&inner.join("deep")), None);
    assert_eq!(find_repo_dir(&outer.join("x")), Some(outer));
}
