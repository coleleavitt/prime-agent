//! The brain's corpus: every memory the agent dir and magic-context hold,
//! as [`BrainDoc`]s under namespaced keys, re-read only when a source file's
//! stamp (mtime, length) changes. The corpus never writes a source.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pa_core::refinement::{HarnessScope, HarnessState, RefinementKind, load_harness_state};
use serde::Serialize;

use super::mc;
use super::project::{project_of, session_cwd};

/// Where a memory lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Source {
    /// A global harness memory (`<agent dir>/harness/harness_state.json`).
    Global,
    /// A session-local harness memory
    /// (`<agent dir>/session-artifacts/<id>/harness/harness_state.json`).
    Session,
    /// An auto-memory file (`<agent dir>/projects/<slug>/memory/*.md`).
    ProjectFile,
    /// A personal memory file (`<agent dir>/memory/personal/*.md`).
    Personal,
    /// An active magic-context row.
    MagicContext,
}

impl Source {
    /// The name the host requests and the skill use.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Session => "session",
            Self::ProjectFile => "file",
            Self::Personal => "personal",
            Self::MagicContext => "magic-context",
        }
    }

    /// The ranking boost (magic-context's per-source boosts, re-weighted
    /// toward the curated harness tiers).
    pub(crate) fn boost(self) -> f64 {
        match self {
            Self::Global => 1.3,
            Self::ProjectFile | Self::Personal => 1.25,
            Self::Session => 1.2,
            Self::MagicContext => 1.1,
        }
    }
}

/// One memory.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrainDoc {
    /// The namespaced key: `global:<id>`, `session:<session>:<id>`,
    /// `file:<slug>/<name>`, `personal:<name>`, `mc:<row id>`.
    pub(crate) key: String,
    pub(crate) source: Source,
    /// The harness entry id (global and session memories): the id flyrag
    /// learns under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// The project's `git:<root commit>` identity, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<String>,
    /// The project's auto-memory slug, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) slug: Option<String>,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    pub(crate) updated_at: String,
    /// A supersession the source itself records (an entry's
    /// `metadata.supersededBy`), as a key.
    #[serde(skip)]
    pub(crate) declared_superseded_by: Option<String>,
}

/// The source locations one corpus reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrainPaths {
    pub(crate) agent_dir: PathBuf,
    /// magic-context's database; `None` skips the source.
    pub(crate) magic_context_db: Option<PathBuf>,
}

/// `PRIME_AGENT_BRAIN_MAGIC_CONTEXT_DB` names another database, or turns the
/// source off (`0`, `off`, `false`, empty).
pub(crate) const MAGIC_CONTEXT_DB_ENV: &str = "PRIME_AGENT_BRAIN_MAGIC_CONTEXT_DB";

impl BrainPaths {
    /// The production paths of `agent_dir`: magic-context's database under
    /// `$XDG_DATA_HOME` (else `~/.local/share`), unless the env var says
    /// otherwise.
    pub(crate) fn for_agent_dir(agent_dir: &Path) -> Self {
        Self {
            agent_dir: agent_dir.to_path_buf(),
            magic_context_db: magic_context_db_from(
                std::env::var(MAGIC_CONTEXT_DB_ENV).ok().as_deref(),
                std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
                std::env::home_dir(),
            ),
        }
    }
}

fn magic_context_db_from(
    setting: Option<&str>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match setting.map(str::trim) {
        Some("" | "0" | "off" | "false" | "no") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => xdg_data_home
            .filter(|dir| dir.is_absolute())
            .or_else(|| home.map(|home| home.join(".local/share")))
            .map(|data| data.join("cortexkit/magic-context/context.db")),
    }
}

/// A source file's stamp; a changed stamp re-reads the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified_ms: u128,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_millis()),
        len: metadata.len(),
    })
}

fn modified_iso(path: &Path) -> String {
    let millis = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    pa_ledger::iso_from_millis(millis)
}

/// What one source file is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FileKind {
    GlobalHarness,
    SessionHarness(String),
    ProjectFile(String),
    Personal,
    MagicContext,
}

/// The cached corpus.
#[derive(Debug, Default)]
pub(crate) struct Corpus {
    files: BTreeMap<PathBuf, (FileKind, Option<Stamp>, Vec<BrainDoc>)>,
    /// session id -> (project identity, slug), resolved once per process.
    session_projects: HashMap<String, (Option<String>, String)>,
    generation: u64,
}

impl Corpus {
    /// Bumped whenever a refresh changed the documents.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Every document, in key order of their files.
    pub(crate) fn docs(&self) -> impl Iterator<Item = &BrainDoc> {
        self.files.values().flat_map(|(_, _, docs)| docs.iter())
    }

    /// Re-read every source whose stamp changed; drop vanished ones.
    /// Returns whether anything changed.
    pub(crate) fn refresh(&mut self, paths: &BrainPaths) -> bool {
        let wanted = enumerate(paths);
        let mut changed = false;
        let before = self.files.len();
        self.files.retain(|path, _| wanted.contains_key(path));
        changed |= self.files.len() != before;
        for (path, kind) in wanted {
            let current = match kind {
                // A WAL-mode database changes in its `-wal` file first.
                FileKind::MagicContext => mc_stamp(&path),
                _ => stamp(&path),
            };
            if self
                .files
                .get(&path)
                .is_some_and(|(_, cached, _)| *cached == current)
            {
                continue;
            }
            let docs = self.load(paths, &path, &kind);
            self.files.insert(path, (kind, current, docs));
            changed = true;
        }
        if changed {
            self.generation += 1;
        }
        changed
    }

    fn load(&mut self, paths: &BrainPaths, path: &Path, kind: &FileKind) -> Vec<BrainDoc> {
        match kind {
            FileKind::GlobalHarness => {
                let dir = path.parent().unwrap_or(path);
                harness_docs(
                    &load_harness_state(dir, HarnessScope::Global),
                    None,
                    (None, None),
                )
            }
            FileKind::SessionHarness(session_id) => {
                let dir = path.parent().unwrap_or(path);
                let state = load_harness_state(dir, HarnessScope::Local);
                if state.entries[&RefinementKind::Memory].is_empty() {
                    return Vec::new();
                }
                let (identity, slug) = self.session_project(&paths.agent_dir, session_id);
                harness_docs(&state, Some(session_id), (identity, Some(slug)))
            }
            FileKind::ProjectFile(slug) => file_doc(path, Source::ProjectFile, Some(slug)),
            FileKind::Personal => file_doc(path, Source::Personal, None),
            FileKind::MagicContext => mc::load(path).unwrap_or_else(|error| {
                tracing::debug!(
                    target: "pa_daemon::retrieval",
                    %error,
                    "magic-context source unreadable; skipped"
                );
                Vec::new()
            }),
        }
    }

    fn session_project(&mut self, agent_dir: &Path, session_id: &str) -> (Option<String>, String) {
        if let Some(project) = self.session_projects.get(session_id) {
            return project.clone();
        }
        let project = session_cwd(agent_dir, session_id)
            .map(|cwd| {
                let project = project_of(&cwd);
                (project.identity, project.slug)
            })
            .unwrap_or_default();
        self.session_projects
            .insert(session_id.to_string(), project.clone());
        project
    }
}

fn mc_stamp(db: &Path) -> Option<Stamp> {
    let main = stamp(db)?;
    let wal = stamp(&PathBuf::from(format!("{}-wal", db.display())));
    Some(Stamp {
        modified_ms: main.modified_ms.max(wal.map_or(0, |wal| wal.modified_ms)),
        len: main.len ^ wal.map_or(0, |wal| wal.len),
    })
}

/// Every source file that exists, with what it is.
fn enumerate(paths: &BrainPaths) -> BTreeMap<PathBuf, FileKind> {
    let agent_dir = &paths.agent_dir;
    let mut out = BTreeMap::new();
    let global = agent_dir.join("harness").join("harness_state.json");
    if global.is_file() {
        out.insert(global, FileKind::GlobalHarness);
    }
    for (session_id, dir) in subdirs(&agent_dir.join("session-artifacts")) {
        let file = dir.join("harness").join("harness_state.json");
        if file.is_file() {
            out.insert(file, FileKind::SessionHarness(session_id));
        }
    }
    for (slug, dir) in subdirs(&agent_dir.join("projects")) {
        for file in markdown_files(&dir.join("memory")) {
            out.insert(file, FileKind::ProjectFile(slug.clone()));
        }
    }
    for file in markdown_files(&agent_dir.join("memory").join("personal")) {
        out.insert(file, FileKind::Personal);
    }
    if let Some(db) = paths.magic_context_db.as_ref().filter(|db| db.is_file()) {
        out.insert(db.clone(), FileKind::MagicContext);
    }
    out
}

fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect()
}

/// The memory files of a directory: `*.md` except the `MEMORY.md` index.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| ext == "md")
                && path.file_name().is_some_and(|name| name != "MEMORY.md")
        })
        .collect()
}

/// The memory entries of one harness state as documents.
fn harness_docs(
    state: &HarnessState,
    session_id: Option<&str>,
    (project, slug): (Option<String>, Option<String>),
) -> Vec<BrainDoc> {
    let key_of = |id: &str| match session_id {
        Some(session) => format!("session:{session}:{id}"),
        None => format!("global:{id}"),
    };
    state.entries[&RefinementKind::Memory]
        .values()
        .map(|entry| BrainDoc {
            key: key_of(&entry.id),
            source: if session_id.is_some() {
                Source::Session
            } else {
                Source::Global
            },
            memory_id: Some(entry.id.clone()),
            session_id: session_id.map(str::to_string),
            project: project.clone(),
            slug: slug.clone(),
            title: entry.title.clone(),
            content: entry.content.clone(),
            path: entry.path.clone(),
            category: entry
                .metadata
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            updated_at: entry.updated_at.clone(),
            declared_superseded_by: entry
                .metadata
                .get("supersededBy")
                .and_then(serde_json::Value::as_str)
                .map(|by| {
                    if by.contains(':') {
                        by.to_string()
                    } else {
                        key_of(by)
                    }
                }),
        })
        .collect()
}

/// One memory file (frontmatter `name`, `description`, `type`, then the
/// body) as a document.
fn file_doc(path: &Path, source: Source, slug: Option<&str>) -> Vec<BrainDoc> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (front, body) = split_frontmatter(&text);
    let field = |name: &str| front.get(name).cloned().filter(|value| !value.is_empty());
    let description = field("description");
    let content = match description {
        Some(description) if !body.trim().is_empty() => {
            format!("{description}\n{}", body.trim())
        }
        Some(description) => description,
        None => body.trim().to_string(),
    };
    vec![BrainDoc {
        key: match slug {
            Some(slug) => format!("file:{slug}/{stem}"),
            None => format!("personal:{stem}"),
        },
        source,
        memory_id: None,
        session_id: None,
        project: None,
        slug: slug.map(str::to_string),
        title: field("name").unwrap_or_else(|| stem.clone()),
        content,
        path: path.display().to_string(),
        category: field("type"),
        updated_at: modified_iso(path),
        declared_superseded_by: None,
    }]
}

/// A leading `---` YAML-ish block's flat `key: value` lines (memory files use
/// no nesting beyond `metadata:` children, which are folded in by name), and
/// the rest of the text.
fn split_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();
    let Some(rest) = text.strip_prefix("---\n") else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text);
    };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.trim().split_once(':') {
            fields
                .entry(key.trim().to_string())
                .or_insert_with(|| value.trim().trim_matches('"').to_string());
        }
    }
    let body = rest[end + 4..].trim_start_matches(['-', '\n']);
    (fields, body)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A harness state file with the given memory entries `(id, title,
    /// content, metadata)`.
    pub(crate) fn write_harness(dir: &Path, entries: &[(&str, &str, &str, serde_json::Value)]) {
        let mut memory = serde_json::Map::new();
        for (id, title, content, metadata) in entries {
            memory.insert(
                (*id).to_string(),
                serde_json::json!({
                    "id": id, "kind": "memory", "title": title, "content": content,
                    "path": format!("memories/{id}.md"), "reference": {}, "arguments": {},
                    "metadata": metadata, "source": "test",
                    "created_at": "2026-11-01T00:00:00.000Z",
                    "updated_at": "2026-11-02T00:00:00.000Z", "version": 1
                }),
            );
        }
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("harness_state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": 1,
                "entries": {"prompt": {}, "memory": memory, "skill": {}, "subagent": {}, "factory": {}},
                "refinements": []
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn paths(agent: &Path) -> BrainPaths {
        BrainPaths {
            agent_dir: agent.to_path_buf(),
            magic_context_db: None,
        }
    }

    #[test]
    fn every_store_becomes_namespaced_documents() {
        let agent = tempfile::tempdir().unwrap();
        let a = agent.path();
        write_harness(
            &a.join("harness"),
            &[("g1", "Global", "use rustc-rdr-main", serde_json::json!({}))],
        );
        write_harness(
            &a.join("session-artifacts/s1/harness"),
            &[(
                "l1",
                "Local",
                "aft shim lease fix",
                serde_json::json!({"supersededBy": "l2"}),
            )],
        );
        std::fs::create_dir_all(a.join("projects/-work-repo/memory")).unwrap();
        std::fs::write(
            a.join("projects/-work-repo/memory/MEMORY.md"),
            "- [x](x.md)\n",
        )
        .unwrap();
        std::fs::write(
            a.join("projects/-work-repo/memory/tooling.md"),
            "---\nname: tooling\ndescription: build with mold\nmetadata:\n  type: project\n---\n\nBody line.\n",
        )
        .unwrap();
        std::fs::create_dir_all(a.join("memory/personal")).unwrap();
        std::fs::write(a.join("memory/personal/preferences.md"), "terse replies\n").unwrap();

        let mut corpus = Corpus::default();
        assert!(corpus.refresh(&paths(a)));
        let mut keys: Vec<_> = corpus.docs().map(|doc| doc.key.clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "file:-work-repo/tooling",
                "global:g1",
                "personal:preferences",
                "session:s1:l1"
            ]
        );
        let file = corpus
            .docs()
            .find(|doc| doc.key == "file:-work-repo/tooling")
            .unwrap();
        assert_eq!(file.content, "build with mold\nBody line.");
        assert_eq!(file.category.as_deref(), Some("project"));
        let local = corpus
            .docs()
            .find(|doc| doc.key == "session:s1:l1")
            .unwrap();
        assert_eq!(
            local.declared_superseded_by.as_deref(),
            Some("session:s1:l2")
        );
        let generation = corpus.generation();
        assert!(
            !corpus.refresh(&paths(a)),
            "unchanged sources re-read nothing"
        );
        assert_eq!(corpus.generation(), generation);
        std::fs::remove_file(a.join("memory/personal/preferences.md")).unwrap();
        assert!(corpus.refresh(&paths(a)));
        assert!(corpus.docs().all(|doc| doc.key != "personal:preferences"));
    }

    #[test]
    fn the_magic_context_database_resolves_from_setting_xdg_then_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(magic_context_db_from(Some("off"), None, home.clone()), None);
        assert_eq!(
            magic_context_db_from(Some("/db/context.db"), None, home.clone()),
            Some(PathBuf::from("/db/context.db"))
        );
        assert_eq!(
            magic_context_db_from(None, Some(PathBuf::from("/xdg")), home.clone()),
            Some(PathBuf::from("/xdg/cortexkit/magic-context/context.db"))
        );
        assert_eq!(
            magic_context_db_from(None, Some(PathBuf::from("relative")), home),
            Some(PathBuf::from(
                "/home/u/.local/share/cortexkit/magic-context/context.db"
            ))
        );
    }
}
