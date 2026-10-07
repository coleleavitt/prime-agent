//! Port of the TS `package-harness.test.ts` mounting, validation,
//! provenance, collision, shadowing, redaction and refinement-guard cases.
use super::*;
use crate::packages::resolve::{PathMetadata, ResourceOrigin};
use crate::refinement::planner::{
    apply_refinement_proposal, ApplyOptions, RefinementEdit, RefinementProposal,
};
use crate::refinement::RefinementAction;
use std::collections::BTreeMap;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn resource(base: &Path, file: &str, source: &str, scope: SourceScope) -> ResolvedResource {
    ResolvedResource {
        path: base.join(file),
        enabled: true,
        metadata: PathMetadata {
            source: MetadataSource::Package(source.to_string()),
            scope,
            origin: ResourceOrigin::Package,
            base_dir: Some(base.to_path_buf()),
        },
    }
}

fn entry_json(kind: &str, id: &str) -> String {
    json!({ "id": id, "kind": kind, "title": format!("{id} title"), "content": format!("{id} body") })
        .to_string()
}

/// A valid memory mounts read-only with its provenance (sanitized source,
/// scope, package-relative file, the package.json revision), the fixed
/// timestamps, and the default path.
#[test]
fn a_valid_entry_mounts_with_provenance() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().join("pkg");
    write(&base.join("package.json"), r#"{"version":"1.2.3"}"#);
    write(
        &base.join("harness/memory/repo_policy.json"),
        &entry_json("memory", "repo_policy"),
    );
    let load = load_package_harness(&[resource(
        &base,
        "harness/memory/repo_policy.json",
        "git:https://user:hunter2@github.com/org/skills.git?token=abc&ref=main",
        SourceScope::User,
    )]);
    let entry = &load.state.entries[&RefinementKind::Memory]["repo_policy"];
    assert_eq!(
        (
            load.diagnostics.len(),
            package_provenance(entry),
            entry.path.clone(),
            entry.created_at.clone(),
            harness_entry_label(entry),
        ),
        (
            0,
            Some(PackageHarnessProvenance {
                origin: "package".to_string(),
                source: "git:https://github.com/org/skills.git?ref=main".to_string(),
                scope: "user".to_string(),
                file: "harness/memory/repo_policy.json".to_string(),
                revision: Some("v1.2.3".to_string()),
                read_only: true,
            }),
            "general".to_string(),
            PACKAGE_HARNESS_TIMESTAMP.to_string(),
            "package:repo_policy".to_string(),
        )
    );
}

/// Invalid files become warnings and never break loading: layout, kind,
/// id, field, and skill-reference violations.
#[test]
fn invalid_entries_become_warnings() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().join("pkg");
    let cases = [
        ("harness/memory/nested/x.json", entry_json("memory", "x")),
        ("harness/tool/x.json", entry_json("tool", "x")),
        (
            "harness/memory/prototype.json",
            entry_json("memory", "prototype"),
        ),
        (
            "harness/memory/mismatch.json",
            entry_json("memory", "other"),
        ),
        (
            "harness/memory/empty.json",
            json!({ "id": "empty", "kind": "memory", "title": " ", "content": "c" }).to_string(),
        ),
        ("harness/skill/tool.json", entry_json("skill", "tool")),
        ("harness/memory/broken.json", "{".to_string()),
    ];
    let resources: Vec<ResolvedResource> = cases
        .iter()
        .map(|(file, body)| {
            write(&base.join(file), body);
            resource(&base, file, "npm:pkg", SourceScope::User)
        })
        .collect();
    let load = load_package_harness(&resources);
    let messages: Vec<String> = load
        .diagnostics
        .iter()
        .map(|diagnostic| match diagnostic {
            ResourceDiagnostic::Warning { message, .. } => message.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .map(|message| message.split(':').next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        messages,
        vec![
            "package harness file must use harness/<kind>/<id>.json".to_string(),
            "package harness path has unsupported kind tool".to_string(),
            "package harness id prototype is reserved".to_string(),
            "package harness entry id must match file id mismatch".to_string(),
            "package harness entry title must be a nonempty string".to_string(),
            "package harness skill reference.type must be python".to_string(),
            "failed to read package harness entry".to_string(),
        ]
    );
    assert!(load.state.entries.values().all(BTreeMap::is_empty));
}

/// Project packages win `(kind, id)` collisions over user packages; the
/// loser is a collision diagnostic.
#[test]
fn project_packages_win_collisions() {
    let dir = tempfile::TempDir::new().unwrap();
    let user = dir.path().join("user-pkg");
    let project = dir.path().join("project-pkg");
    for base in [&user, &project] {
        write(
            &base.join("harness/prompt/note.json"),
            &entry_json("prompt", "note"),
        );
    }
    let load = load_package_harness(&[
        resource(
            &user,
            "harness/prompt/note.json",
            "npm:user-pkg",
            SourceScope::User,
        ),
        resource(
            &project,
            "harness/prompt/note.json",
            "npm:project-pkg",
            SourceScope::Project,
        ),
    ]);
    let winner = &load.state.entries[&RefinementKind::Prompt]["note"];
    assert_eq!(
        (
            package_provenance(winner).map(|provenance| provenance.source),
            winner.path.clone(),
            load.diagnostics,
        ),
        (
            Some("npm:project-pkg".to_string()),
            "policy".to_string(),
            vec![ResourceDiagnostic::Collision {
                message: "package harness prompt:note collision; keeping npm:project-pkg"
                    .to_string(),
                path: user.join("harness/prompt/note.json").display().to_string(),
                collision: ResourceCollision {
                    resource_type: "harness",
                    name: "prompt:note".to_string(),
                    winner_path: "npm:project-pkg#harness/prompt/note.json".to_string(),
                    loser_path: "npm:user-pkg#harness/prompt/note.json".to_string(),
                },
            }],
        )
    );
}

/// A local package renders as `local:<dirname>` (never its path); the git
/// revision reads HEAD through the packed refs without a subprocess.
#[test]
fn local_sources_hide_paths_and_git_revisions_read_packed_refs() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().join("my-skills");
    write(
        &base.join("harness/memory/m.json"),
        &entry_json("memory", "m"),
    );
    write(&base.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(
        &base.join(".git/packed-refs"),
        "# pack-refs\n0123456789abcdef0123456789abcdef01234567 refs/heads/main\n",
    );
    let load = load_package_harness(&[resource(
        &base,
        "harness/memory/m.json",
        &base.display().to_string(),
        SourceScope::Project,
    )]);
    let provenance = package_provenance(&load.state.entries[&RefinementKind::Memory]["m"]).unwrap();
    assert_eq!(
        (provenance.source, provenance.revision),
        (
            "local:my-skills".to_string(),
            Some("0123456789ab".to_string())
        )
    );
    assert_eq!(
        package_provenance_text(&load.state.entries[&RefinementKind::Memory]["m"], 240),
        " [read-only package; scope=project; source=local:my-skills rev=0123456789ab; file=harness/memory/m.json]"
    );
}

#[test]
fn package_sources_redact_credentials() {
    assert_eq!(
        [
            sanitize_package_source("https://x:secret@example.com/pkg.git?access_token=1&ref=v1"),
            sanitize_package_source("git:deploy:pw@github.com:org/repo.git"),
            sanitize_package_source("git:git@github.com:org/repo.git"),
            sanitize_package_source("npm:@scope/pkg"),
        ],
        [
            "https://example.com/pkg.git?ref=v1".to_string(),
            "git:github.com:org/repo.git".to_string(),
            "git:git@github.com:org/repo.git".to_string(),
            "npm:@scope/pkg".to_string(),
        ]
    );
}

/// An editable entry with the same `(kind, id)` shadows the package entry;
/// other package entries overlay below the editable ones.
#[test]
fn editable_entries_shadow_package_entries() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().join("pkg");
    for id in ["shared", "only_pkg"] {
        write(
            &base.join(format!("harness/memory/{id}.json")),
            &entry_json("memory", id),
        );
    }
    let load = load_package_harness(&[
        resource(
            &base,
            "harness/memory/shared.json",
            "npm:pkg",
            SourceScope::User,
        ),
        resource(
            &base,
            "harness/memory/only_pkg.json",
            "npm:pkg",
            SourceScope::User,
        ),
    ]);
    let mut merged = empty_harness_state();
    let mut editable = load.state.entries[&RefinementKind::Memory]["shared"].clone();
    editable.extensions.clear();
    editable.content = "editable".to_string();
    merged
        .entries
        .get_mut(&RefinementKind::Memory)
        .unwrap()
        .insert("shared".to_string(), editable);
    overlay_package_harness(&mut merged, &load.state);
    let memories = &merged.entries[&RefinementKind::Memory];
    assert_eq!(
        (
            memories["shared"].content.clone(),
            is_package_entry(&memories["shared"]),
            is_package_entry(&memories["only_pkg"]),
        ),
        ("editable".to_string(), false, true)
    );
}

/// `/refine` refuses update and delete edits against a package-only id;
/// a create with the same `(kind, id)` lands as an editable override.
#[test]
fn refine_refuses_to_mutate_package_entries() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().join("pkg");
    write(
        &base.join("harness/memory/policy.json"),
        &entry_json("memory", "policy"),
    );
    let load = load_package_harness(&[resource(
        &base,
        "harness/memory/policy.json",
        "npm:pkg",
        SourceScope::User,
    )]);
    let edit = |action: RefinementAction| -> RefinementEdit {
        serde_json::from_value(json!({
            "action": action, "kind": "memory", "id": "policy",
            "title": "Override", "content": "local override", "reason": "test",
        }))
        .unwrap()
    };
    let mut state = empty_harness_state();
    let proposal = RefinementProposal {
        summary: "s".to_string(),
        rationale: "r".to_string(),
        expected_outcome: "e".to_string(),
        edits: vec![
            edit(RefinementAction::Update),
            edit(RefinementAction::Delete),
            edit(RefinementAction::Create),
        ],
    };
    let result = apply_refinement_proposal(
        &mut state,
        &proposal,
        ApplyOptions {
            id: "refine_1".to_string(),
            rollback_of: None,
            scope: Some(crate::refinement::HarnessScope::Local),
            baseline_state: None,
            factory_enabled: false,
            package_state: Some(std::sync::Arc::new(load.state)),
        },
    );
    let outcomes: Vec<(bool, Option<String>)> = result
        .applied_edits
        .iter()
        .map(|edit| (edit.applied, edit.error.clone()))
        .collect();
    let refused = Some(
        "package harness entry is read-only; create an editable same-kind, same-id override instead"
            .to_string(),
    );
    assert_eq!(
        (
            outcomes,
            state.entries[&RefinementKind::Memory]["policy"]
                .content
                .clone()
        ),
        (
            vec![(false, refused.clone()), (false, refused), (true, None)],
            "local override".to_string(),
        )
    );
}
