//! Search over the brain's corpus: BM25 over title + content, then
//! magic-context-style boosts (source, current project), flyrag's learned
//! term, the user's pins, and supersession. Excluded memories never surface;
//! superseded ones only on request, ranked down and labelled.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::Serialize;

use super::controls::MemoryControls;
use super::corpus::{BrainDoc, BrainPaths, Corpus, Source};
use super::project::Project;
use super::supersede::{self, Item, Superseded};
use crate::Query;
use crate::bm25::Bm25Index;

/// Results returned when the caller names no limit.
pub(crate) const DEFAULT_LIMIT: usize = 10;
/// The most results one search returns.
pub(crate) const MAX_LIMIT: usize = 25;
/// Characters of content a hit previews (`brain.get` returns all of it).
pub(crate) const PREVIEW_CHARS: usize = 220;
/// The factor a superseded memory's score takes.
const SUPERSEDED_FACTOR: f64 = 0.3;
/// The factor a memory of the current project takes.
const PROJECT_FACTOR: f64 = 1.2;
/// The factor a pinned memory takes.
const PINNED_FACTOR: f64 = 1.5;
/// The weight of flyrag's learned term.
const LEARNED_WEIGHT: f64 = 0.25;

/// Which memories a search covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Everything.
    All,
    /// The current project: its sessions, auto-memory files and
    /// magic-context rows, plus the global memories.
    Project,
    /// Global harness memories and personal files.
    Global,
    /// This session's local memories.
    Session,
    /// Auto-memory and personal files.
    Files,
    /// magic-context rows.
    MagicContext,
    /// Compartments of every session's folded history.
    History,
}

impl Scope {
    /// The scope a request names.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "all" | "any" | "" => Self::All,
            "project" => Self::Project,
            "global" => Self::Global,
            "session" => Self::Session,
            "files" => Self::Files,
            "magic-context" | "magic_context" | "mc" => Self::MagicContext,
            "history" | "compartments" => Self::History,
            _ => return None,
        })
    }
}

/// Where a search runs from.
#[derive(Clone)]
pub(crate) struct SearchContext<'a> {
    pub(crate) project: &'a Project,
    pub(crate) session_id: &'a str,
    pub(crate) controls: &'a MemoryControls,
    /// flyrag's learned term of a harness memory id.
    pub(crate) learned: &'a dyn Fn(&str) -> f32,
}

/// One search.
#[derive(Debug, Clone)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) scope: Scope,
    pub(crate) limit: usize,
    pub(crate) include_superseded: bool,
}

/// One result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Hit {
    pub(crate) key: String,
    pub(crate) source: Source,
    pub(crate) title: String,
    pub(crate) preview: String,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    pub(crate) updated_at: String,
    pub(crate) score: f64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) superseded: Option<Superseded>,
}

/// The corpus with its index, rebuilt when the corpus or the recorded
/// supersessions change.
#[derive(Debug)]
pub(crate) struct Brain {
    paths: BrainPaths,
    corpus: Corpus,
    index: Bm25Index,
    built: Option<(u64, BTreeMap<String, String>)>,
    superseded: BTreeMap<String, Superseded>,
}

impl Brain {
    pub(crate) fn new(paths: BrainPaths) -> Self {
        Self {
            paths,
            corpus: Corpus::default(),
            index: Bm25Index::new(),
            built: None,
            superseded: BTreeMap::new(),
        }
    }

    /// Re-read changed sources and rebuild what depends on them.
    pub(crate) fn refresh(&mut self, recorded: &BTreeMap<String, String>) {
        self.corpus.refresh(&self.paths);
        let generation = self.corpus.generation();
        if self.built.as_ref().is_some_and(|(built, built_recorded)| {
            *built == generation && built_recorded == recorded
        }) {
            return;
        }
        let mut index = Bm25Index::new();
        for doc in self.corpus.docs() {
            index.upsert(&doc.key, &format!("{}\n{}", doc.title, doc.content));
        }
        self.index = index;
        let items: Vec<Item<'_>> = self
            .corpus
            .docs()
            .map(|doc| Item {
                key: &doc.key,
                name: doc
                    .memory_id
                    .as_deref()
                    .unwrap_or_else(|| doc.key.rsplit(['/', ':']).next().unwrap_or(&doc.key)),
                content: &doc.content,
                updated_at: &doc.updated_at,
                declared: doc.declared_superseded_by.as_deref(),
            })
            .collect();
        self.superseded = supersede::superseded(&items, recorded);
        self.built = Some((generation, recorded.clone()));
    }

    /// The supersessions of the current corpus.
    pub(crate) fn superseded(&self) -> &BTreeMap<String, Superseded> {
        &self.superseded
    }

    /// Every document.
    pub(crate) fn docs(&self) -> impl Iterator<Item = &BrainDoc> {
        self.corpus.docs()
    }

    /// The document named `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&BrainDoc> {
        self.corpus.docs().find(|doc| doc.key == key)
    }

    /// The best `request.limit` memories for `request.query`.
    pub(crate) fn search(&self, request: &SearchRequest, context: &SearchContext<'_>) -> Vec<Hit> {
        let by_key: HashMap<&str, &BrainDoc> = self
            .corpus
            .docs()
            .map(|doc| (doc.key.as_str(), doc))
            .collect();
        let candidates = self
            .index
            .top_k(&Query::from_text(request.query.clone()), by_key.len());
        let in_scope: Vec<(&BrainDoc, f64)> = candidates
            .iter()
            .filter_map(|candidate| {
                let doc = by_key.get(candidate.id.as_str())?;
                (in_scope(doc, request.scope, context)
                    && !context.controls.excluded.contains(&doc.key)
                    && (request.include_superseded || !self.superseded.contains_key(&doc.key)))
                .then_some((*doc, candidate.base_score))
            })
            .collect();
        let top = in_scope
            .iter()
            .map(|(_, score)| *score)
            .fold(0.0_f64, f64::max);
        if top <= 0.0 {
            return Vec::new();
        }
        let mut hits: Vec<Hit> = in_scope
            .into_iter()
            .map(|(doc, base)| {
                let pinned = context.controls.pinned.contains(&doc.key);
                let superseded = self.superseded.get(&doc.key).cloned();
                let learned = doc
                    .memory_id
                    .as_deref()
                    .map_or(0.0, |id| f64::from((context.learned)(id)));
                let mut score = base / top * doc.source.boost() * (1.0 + LEARNED_WEIGHT * learned);
                if context
                    .project
                    .contains(doc.project.as_deref(), doc.slug.as_deref())
                {
                    score *= PROJECT_FACTOR;
                }
                if pinned {
                    score *= PINNED_FACTOR;
                }
                if superseded.is_some() {
                    score *= SUPERSEDED_FACTOR;
                }
                Hit {
                    key: doc.key.clone(),
                    source: doc.source,
                    title: doc.title.clone(),
                    preview: preview(&doc.content),
                    path: doc.path.clone(),
                    project: doc.project.clone(),
                    session_id: doc.session_id.clone(),
                    category: doc.category.clone(),
                    updated_at: doc.updated_at.clone(),
                    score: (score * 10_000.0).round() / 10_000.0,
                    pinned,
                    superseded,
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.key.cmp(&b.key))
        });
        hits.truncate(request.limit.clamp(1, MAX_LIMIT));
        hits
    }
}

fn in_scope(doc: &BrainDoc, scope: Scope, context: &SearchContext<'_>) -> bool {
    match scope {
        Scope::All => true,
        Scope::Project => {
            matches!(doc.source, Source::Global | Source::Personal)
                || context
                    .project
                    .contains(doc.project.as_deref(), doc.slug.as_deref())
        }
        Scope::Global => matches!(doc.source, Source::Global | Source::Personal),
        Scope::Session => doc.session_id.as_deref() == Some(context.session_id),
        Scope::Files => matches!(doc.source, Source::ProjectFile | Source::Personal),
        Scope::MagicContext => doc.source == Source::MagicContext,
        Scope::History => doc.source == Source::Compartment,
    }
}

/// The first [`PREVIEW_CHARS`] characters of `content`, on one line.
pub(crate) fn preview(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_CHARS - 3).collect();
    format!("{cut}...")
}

/// The process's brain for `agent_dir` (one per agent dir, like the
/// reranker handle).
pub(crate) fn shared_brain(agent_dir: &Path) -> Arc<Mutex<Brain>> {
    static BRAINS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Brain>>>>> = OnceLock::new();
    let mut brains = BRAINS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        brains.entry(agent_dir.to_path_buf()).or_insert_with(|| {
            Arc::new(Mutex::new(Brain::new(BrainPaths::for_agent_dir(agent_dir))))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::corpus::tests::write_harness;

    fn fixture() -> (tempfile::TempDir, Brain) {
        let agent = tempfile::tempdir().unwrap();
        let a = agent.path();
        write_harness(
            &a.join("harness"),
            &[
                (
                    "lease_fix",
                    "AFT shim lease",
                    "per-pid XDG dirs fix the writer lease timeout",
                    serde_json::json!({}),
                ),
                (
                    "toolchain",
                    "Toolchain",
                    "build with rustc-rdr-main and mold",
                    serde_json::json!({}),
                ),
                (
                    "handoff_part_1",
                    "Handoff 1",
                    "writer lease investigation started",
                    serde_json::json!({}),
                ),
                (
                    "handoff_part_2",
                    "Handoff 2",
                    "writer lease investigation finished",
                    serde_json::json!({}),
                ),
            ],
        );
        write_harness(
            &a.join("session-artifacts/s9/harness"),
            &[(
                "session_note",
                "Session note",
                "the writer lease is held per data dir",
                serde_json::json!({}),
            )],
        );
        let mut brain = Brain::new(BrainPaths {
            agent_dir: a.to_path_buf(),
            magic_context_db: None,
        });
        brain.refresh(&BTreeMap::new());
        (agent, brain)
    }

    fn request(query: &str, scope: Scope) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            scope,
            limit: DEFAULT_LIMIT,
            include_superseded: false,
        }
    }

    fn context<'a>(
        project: &'a Project,
        controls: &'a MemoryControls,
        learned: &'a dyn Fn(&str) -> f32,
    ) -> SearchContext<'a> {
        SearchContext {
            project,
            session_id: "s1",
            controls,
            learned,
        }
    }

    fn project() -> Project {
        Project {
            identity: None,
            slug: "-nowhere".to_string(),
        }
    }

    #[test]
    fn search_spans_sessions_and_hides_superseded_parts() {
        let (_agent, brain) = fixture();
        let project = project();
        let controls = MemoryControls::default();
        let none = |_: &str| 0.0;
        let hits = brain.search(
            &request("writer lease", Scope::All),
            &context(&project, &controls, &none),
        );
        let keys: Vec<_> = hits.iter().map(|hit| hit.key.as_str()).collect();
        assert!(keys.contains(&"global:lease_fix"));
        assert!(
            keys.contains(&"session:s9:session_note"),
            "another session's memory is found"
        );
        assert!(keys.contains(&"global:handoff_part_2"));
        assert!(
            !keys.contains(&"global:handoff_part_1"),
            "an older series part is hidden"
        );

        let mut with_superseded = request("writer lease", Scope::All);
        with_superseded.include_superseded = true;
        let hits = brain.search(&with_superseded, &context(&project, &controls, &none));
        let old = hits
            .iter()
            .find(|hit| hit.key == "global:handoff_part_1")
            .unwrap();
        assert_eq!(old.superseded.as_ref().unwrap().by, "global:handoff_part_2");
    }

    #[test]
    fn pins_excludes_learning_and_scope_shape_the_ranking() {
        let (_agent, brain) = fixture();
        let project = project();
        let none = |_: &str| 0.0;
        let mut controls = MemoryControls::default();
        controls.excluded.insert("global:lease_fix".to_string());
        controls
            .pinned
            .insert("session:s9:session_note".to_string());
        let hits = brain.search(
            &request("writer lease", Scope::All),
            &context(&project, &controls, &none),
        );
        assert_eq!(hits[0].key, "session:s9:session_note");
        assert!(hits[0].pinned);
        assert!(hits.iter().all(|hit| hit.key != "global:lease_fix"));

        let controls = MemoryControls::default();
        let misled = |id: &str| if id == "lease_fix" { -1.0 } else { 0.0 };
        let hits = brain.search(
            &request("writer lease", Scope::Global),
            &context(&project, &controls, &misled),
        );
        assert!(hits.iter().all(|hit| hit.source == Source::Global));
        assert_ne!(
            hits[0].key, "global:lease_fix",
            "a misleading memory ranks down"
        );

        assert!(
            brain
                .search(
                    &request("writer lease", Scope::Session),
                    &context(&project, &controls, &none)
                )
                .is_empty(),
            "session s1 has no memories of its own"
        );
        assert!(
            brain
                .search(
                    &request("zzzz", Scope::All),
                    &context(&project, &controls, &none)
                )
                .is_empty()
        );
    }

    #[test]
    fn previews_are_bounded_and_flat() {
        assert_eq!(preview("a\n  b"), "a b");
        let long = "x".repeat(500);
        assert_eq!(preview(&long).chars().count(), PREVIEW_CHARS);
    }
}
