//! The trajectory store and its two levers (TS
//! `test/trajectory-index-store.test.ts` and
//! `test/suite/regressions/eti-trajectory-prompt.test.ts`): the store's
//! write/read contract, the digest hook (Lever 1) and the recurrence filter
//! (Lever 2), each absent with the kill switch off or nothing sealed.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::refinement::prompt_hook::{HarnessPromptHook, HarnessPromptHooks};
use pa_core::refinement::ranking::{format_harness_state_for_prompt, HarnessStatePromptOptions};
use pa_core::refinement::{HarnessEntry, HarnessScope, HarnessState, RefinementKind};
use pa_learning::{
    read_trajectory_index, trajectory_index_path, trajectory_prompt_adjustment,
    write_trajectory_index, LearningFeature, TrajectoryLabel, TrajectoryLabelKind,
    TrajectoryStoreFile, TrajectoryWindow, DEFAULT_MAX_TRAJECTORY_WINDOWS, TRAJECTORY_INDEX_ENV,
};
use serde_json::json;

/// The kill switch is process state: tests touching it run one at a time.
static ENV: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `body` with the kill switch set to `value` (unset for `None`).
fn with_switch<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    let previous = std::env::var_os(TRAJECTORY_INDEX_ENV);
    match value {
        Some(value) => std::env::set_var(TRAJECTORY_INDEX_ENV, value),
        None => std::env::remove_var(TRAJECTORY_INDEX_ENV),
    }
    let result = body();
    match previous {
        Some(previous) => std::env::set_var(TRAJECTORY_INDEX_ENV, previous),
        None => std::env::remove_var(TRAJECTORY_INDEX_ENV),
    }
    result
}

fn window(key: &str, corpus: &str) -> TrajectoryWindow {
    TrajectoryWindow {
        window: key.to_string(),
        sealed_at: String::new(),
        days: Vec::new(),
        turns: 0,
        corpus: corpus.to_string(),
        fingerprints: Vec::new(),
    }
}

fn label(
    fingerprint: &str,
    name: &str,
    kind: TrajectoryLabelKind,
    security_class: bool,
) -> TrajectoryLabel {
    TrajectoryLabel {
        fingerprint: fingerprint.to_string(),
        name: name.to_string(),
        message: name.to_string(),
        corpus: "prime".to_string(),
        label: Some(kind),
        withheld: None,
        since_window: "2026-W20".to_string(),
        last_window: "2026-W21".to_string(),
        windows_present: 2,
        windows_recurring: 2,
        claimed_by_refinement: false,
        domain_active: true,
        security_class,
        confounds: vec!["task-mix".to_string()],
    }
}

fn file_with(windows: Vec<TrajectoryWindow>, labels: Vec<TrajectoryLabel>) -> TrajectoryStoreFile {
    TrajectoryStoreFile {
        sealed_at: "1970-01-01T00:00:00.000Z".to_string(),
        windows_observed: 4,
        min_windows: 4,
        windows,
        labels,
        rate: Vec::new(),
    }
}

#[test]
fn the_store_round_trips_owner_only_with_its_gitignore() {
    let agent = tempfile::tempdir().unwrap();
    assert_eq!(read_trajectory_index(agent.path()), None);
    let file = file_with(
        vec![window("2025-W02", "prime"), window("2025-W03", "prime")],
        vec![label(
            "fp1",
            "kernel.cell",
            TrajectoryLabelKind::Persists,
            false,
        )],
    );
    write_trajectory_index(&file, agent.path());
    assert_eq!(read_trajectory_index(agent.path()), Some(file));
    let gitignore = std::fs::read_to_string(agent.path().join("learning/.gitignore")).unwrap();
    assert_eq!(gitignore, "trajectory.json\nbackfill/\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            (
                mode(&trajectory_index_path(agent.path())),
                mode(&agent.path().join("learning"))
            ),
            (0o600, 0o700)
        );
    }
    // A second save wins, and the gitignore is not duplicated.
    let second = file_with(
        vec![
            window("2025-W02", "prime"),
            window("2025-W03", "prime"),
            window("2025-W04", "prime"),
        ],
        Vec::new(),
    );
    write_trajectory_index(&second, agent.path());
    assert_eq!(read_trajectory_index(agent.path()), Some(second));
    assert_eq!(
        std::fs::read_to_string(agent.path().join("learning/.gitignore")).unwrap(),
        gitignore
    );
}

#[test]
fn a_corrupt_oversized_or_foreign_store_reads_as_absent() {
    let agent = tempfile::tempdir().unwrap();
    let path = trajectory_index_path(agent.path());
    write_trajectory_index(
        &file_with(vec![window("2025-W02", "prime")], Vec::new()),
        agent.path(),
    );
    let mut wrong_version = file_with(Vec::new(), Vec::new()).to_json();
    wrong_version["version"] = json!(999);
    let mut empty_confounds = file_with(
        Vec::new(),
        vec![label("fp1", "x", TrajectoryLabelKind::New, false)],
    )
    .to_json();
    empty_confounds["labels"][0]["confounds"] = json!([]);
    for content in [
        "{ not json at all".to_string(),
        wrong_version.to_string(),
        empty_confounds.to_string(),
        "x".repeat(9 * 1024 * 1024),
    ] {
        std::fs::write(&path, &content).unwrap();
        assert_eq!(
            read_trajectory_index(agent.path()),
            None,
            "{}",
            &content[..20.min(content.len())]
        );
    }
}

#[test]
fn the_store_keeps_the_newest_windows_and_never_a_backfill_datum() {
    let agent = tempfile::tempdir().unwrap();
    let mut windows: Vec<TrajectoryWindow> = (2020..=2021)
        .flat_map(|year| (1..=40).map(move |week| window(&format!("{year}-W{week:02}"), "prime")))
        .collect();
    windows.push(window("2021-W41", "backfill:opencode"));
    let mut backfill = label("bf1", "bash", TrajectoryLabelKind::Persists, false);
    backfill.corpus = "backfill:opencode".to_string();
    let file = file_with(
        windows,
        vec![
            label("fp1", "kernel.cell", TrajectoryLabelKind::Persists, false),
            backfill,
        ],
    );
    write_trajectory_index(&file, agent.path());
    let loaded = read_trajectory_index(agent.path()).unwrap();
    let keys: Vec<&str> = loaded
        .windows
        .iter()
        .map(|window| window.window.as_str())
        .collect();
    assert_eq!(keys.len(), DEFAULT_MAX_TRAJECTORY_WINDOWS);
    assert_eq!((keys[0], keys[keys.len() - 1]), ("2020-W29", "2021-W40"));
    assert_eq!(loaded.labels, file.labels[..1]);
    let text = std::fs::read_to_string(trajectory_index_path(agent.path())).unwrap();
    assert!(!text.contains("backfill:"));
}

fn memory(id: &str, title: &str, content: &str) -> HarnessEntry {
    HarnessEntry {
        id: id.to_string(),
        kind: RefinementKind::Memory,
        title: title.to_string(),
        content: content.to_string(),
        path: format!("memories/{id}"),
        scope: Some(HarnessScope::Global),
        reference: serde_json::Map::new(),
        arguments: serde_json::Map::new(),
        metadata: serde_json::Map::new(),
        source: "agent".to_string(),
        created_at: String::new(),
        updated_at: String::new(),
        version: 1,
    }
}

/// One high-relevance memory, three mid ones and one with no query overlap
/// (the TS `seededState`, sized to the native three slots).
fn seeded_state() -> HarnessState {
    let mut state = pa_core::refinement::empty_harness_state();
    let memories = state.entries.get_mut(&RefinementKind::Memory).unwrap();
    memories.insert(
        "e_high".into(),
        memory(
            "e_high",
            "worktree branch layout",
            "How to lay out a worktree for parallel worktree work.",
        ),
    );
    for index in 0..3 {
        let id = format!("e_mid_{index}");
        memories.insert(
            id.clone(),
            memory(
                &id,
                &format!("worktree note {index}"),
                &format!("Mid-relevance note {index}."),
            ),
        );
    }
    memories.insert(
        "e_low".into(),
        memory("e_low", "git hygiene reminder", "Commit incrementally."),
    );
    state
}

fn rendered_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|line| line.strip_prefix("- [global:"))
        .filter_map(|rest| rest.split(']').next())
        .map(str::to_string)
        .collect()
}

fn render(state: &HarnessState, file: Option<&TrajectoryStoreFile>) -> String {
    format_harness_state_for_prompt(
        state,
        &HarnessStatePromptOptions {
            query_terms: Some([("worktree".to_string(), 3.0)].into_iter().collect()),
            adjustment: file
                .map(|file| trajectory_prompt_adjustment(file, state))
                .filter(|adjustment| !adjustment.is_empty()),
            ..HarnessStatePromptOptions::default()
        },
    )
}

/// Lever 1: through the trust-window join a stable-gap entry wins a slot and
/// an internalized one sinks past the slots; up to three stable-gap lines
/// render, each sanitized.
#[test]
fn the_digest_promotes_stable_gaps_sinks_internalized_entries_and_lists_the_gaps() {
    let mut state = seeded_state();
    let baseline = rendered_ids(&render(&state, None));
    assert_eq!(baseline, ["e_high", "e_mid_0", "e_mid_1"]);
    state.extensions.insert(
        "trustWindows".to_string(),
        json!({
            "p1": { "proposalId": "p1", "touched": ["memory:e_low"], "claimedFingerprints": ["a1b2c3d4e5f60718"] },
            "p2": { "proposalId": "p2", "touched": ["memory:e_high"], "claimedFingerprints": ["0011223344556677"] }
        }),
    );
    let mut gaps: Vec<TrajectoryLabel> = [
        "git-hygiene",
        "verify\nwith <script>alert(1)</script>",
        "credential-hygiene",
        "a fourth line that must be dropped",
    ]
    .iter()
    .enumerate()
    .map(|(index, name)| {
        let mut gap = label(
            &format!("{index:016x}"),
            name,
            TrajectoryLabelKind::Persists,
            false,
        );
        gap.windows_recurring = 4 - index as u64;
        gap
    })
    .collect();
    gaps[0].fingerprint = "a1b2c3d4e5f60718".to_string();
    gaps.push(label(
        "0011223344556677",
        "rebase etiquette",
        TrajectoryLabelKind::Dropped,
        false,
    ));
    gaps.push(label(
        "span:0123456789abcdef",
        "span key",
        TrajectoryLabelKind::Persists,
        false,
    ));
    let file = file_with(Vec::new(), gaps);
    let prompt = render(&state, Some(&file));
    assert_eq!(rendered_ids(&prompt), ["e_low", "e_mid_0", "e_mid_1"]);
    let section: Vec<&str> = prompt
        .lines()
        .skip_while(|line| !line.starts_with("engineer trajectory"))
        .take(5)
        .collect();
    assert_eq!(
        section,
        [
            "engineer trajectory (confound-flagged; local signal, may reflect task-mix):",
            "- stable-gap: git-hygiene recurs in 4 of 4 windows [confounds: task-mix]",
            "- stable-gap: verify with &lt;script&gt;alert(1)&lt;/script&gt; recurs in 3 of 4 windows [confounds: task-mix]",
            "- stable-gap: credential-hygiene recurs in 2 of 4 windows [confounds: task-mix]",
            "",
        ]
    );
    // No stable gap and no join: the digest is the native one, byte for byte.
    let quiet = file_with(
        Vec::new(),
        vec![label("ffff", "x", TrajectoryLabelKind::New, false)],
    );
    assert_eq!(
        render(&seeded_state(), Some(&quiet)),
        render(&seeded_state(), None)
    );
}

fn context(agent_dir: &Path) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: agent_dir.to_path_buf(),
        cwd: agent_dir.to_path_buf(),
        session_id: "s1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    })
}

fn hook_render(agent_dir: &Path) -> String {
    let hook = LearningFeature
        .harness_prompt_hook(&context(agent_dir))
        .expect("the feature offers a digest hook");
    let hooks = HarnessPromptHooks(vec![hook as Arc<dyn HarnessPromptHook>]);
    let state = seeded_state();
    format_harness_state_for_prompt(
        &state,
        &HarnessStatePromptOptions {
            adjustment: hooks.adjust(&state),
            ..HarnessStatePromptOptions::default()
        },
    )
}

/// The feature's hook reads the sealed store per render and adds nothing
/// with the kill switch off or nothing sealed.
#[test]
fn the_feature_hook_reads_the_store_and_honours_the_kill_switch() {
    let agent = tempfile::tempdir().unwrap();
    let native =
        format_harness_state_for_prompt(&seeded_state(), &HarnessStatePromptOptions::default());
    assert_eq!(with_switch(None, || hook_render(agent.path())), native);
    write_trajectory_index(
        &file_with(
            Vec::new(),
            vec![label(
                "a1b2c3d4e5f60718",
                "git-hygiene",
                TrajectoryLabelKind::Persists,
                false,
            )],
        ),
        agent.path(),
    );
    let adjusted = with_switch(None, || hook_render(agent.path()));
    assert!(
        adjusted.contains(
            "\n- stable-gap: git-hygiene recurs in 2 of 4 windows [confounds: task-mix]\n"
        ),
        "{adjusted}"
    );
    assert_eq!(
        with_switch(Some("off"), || hook_render(agent.path())),
        native
    );
}

fn muted(agent_dir: &Path, live: &[&str]) -> HashSet<String> {
    let live: Vec<String> = live.iter().map(|id| (*id).to_string()).collect();
    LearningFeature
        .recurrence_filter()
        .muted(&context(agent_dir), &live)
}

/// Lever 2: an internalized fingerprint is muted, a security-class one never
/// is, a live recurrence overrides, and the kill switch or an absent store
/// mutes nothing.
#[test]
fn the_recurrence_filter_mutes_internalized_fingerprints_only() {
    let agent = tempfile::tempdir().unwrap();
    assert_eq!(
        with_switch(None, || muted(agent.path(), &[])),
        HashSet::new()
    );
    write_trajectory_index(
        &file_with(
            Vec::new(),
            vec![
                label(
                    "a1b2c3d4e5f60718",
                    "tool_error: git push rejected",
                    TrajectoryLabelKind::Dropped,
                    false,
                ),
                label(
                    "00ffeeddccbbaa99",
                    "auth token refresh failed",
                    TrajectoryLabelKind::Dropped,
                    true,
                ),
                label(
                    "span:0123456789abcdef",
                    "span key",
                    TrajectoryLabelKind::Dropped,
                    false,
                ),
                label(
                    "1111111111111111",
                    "still here",
                    TrajectoryLabelKind::Persists,
                    false,
                ),
            ],
        ),
        agent.path(),
    );
    let only = HashSet::from(["a1b2c3d4e5f60718".to_string()]);
    assert_eq!(with_switch(None, || muted(agent.path(), &[])), only);
    assert_eq!(
        with_switch(None, || muted(agent.path(), &["a1b2c3d4e5f60718"])),
        HashSet::new()
    );
    assert_eq!(
        with_switch(Some("0"), || muted(agent.path(), &[])),
        HashSet::new()
    );
}
