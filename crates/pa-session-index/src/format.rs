//! The two on-disk tiers, line-compatible with the TS product's
//! `session-catalog-index.ts`: a `{"version":2}` header, then one JSON object
//! per session file. `session-index.ndjson` carries the display metadata
//! (`{file, size, mtimeMs, info}`, `info: null` for a file that is not a
//! session); `session-search-index.ndjson` carries the transcript corpus
//! (`{file, size, mtimeMs, searchText}`).
//!
//! This crate adds two keys the TS reader ignores: `foldVersion` on every
//! line (the native fold's version the row was derived under) and
//! `info.thinkingLevel` (a field the TS row does not have). A line without
//! `foldVersion` was written by the TS product, whose fold differs from the
//! native one (its `created` is re-serialized, its usage rules predate the
//! native ones), so it is never served: the file is folded once and its line
//! rewritten.

use std::collections::HashMap;

use pa_core::session::catalog_cache::{CatalogEntry, CatalogSessionRow, CatalogUsage};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// The metadata tier's file name inside the session directory. The
/// `.ndjson` extension keeps it out of the `.jsonl` session filter.
pub const SESSION_INDEX_FILE: &str = "session-index.ndjson";

/// The corpus tier's file name inside the session directory.
pub const SESSION_SEARCH_INDEX_FILE: &str = "session-search-index.ndjson";

/// TS `SESSION_CATALOG_INDEX_VERSION`: a header with any other version
/// yields no entries.
const FORMAT_VERSION: u32 = 2;

/// One indexed file: its key, the fold version its entry was derived
/// under, and the entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexedEntry {
    pub(crate) size: u64,
    pub(crate) mtime_ms: f64,
    pub(crate) fold_version: u32,
    pub(crate) entry: CatalogEntry,
}

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
}

/// Serialize a whole-number `f64` as an integer, the way `JSON.stringify`
/// prints it (`1789177896715`, not `1789177896715.0`).
// serde's `serialize_with` hands the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn js_number<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        // In range and whole: the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

/// Parse a JSON number from its text with std's correctly rounded parse:
/// `serde_json`'s default float parse is best-effort and can land one ULP
/// off, which would change a served cost or miss an `mtimeMs` key.
fn exact_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    raw.get().parse().map_err(serde::de::Error::custom)
}

#[derive(Serialize, Deserialize)]
struct State<S> {
    status: S,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Model<S> {
    provider: S,
    model_id: S,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(serialize_with = "js_number", deserialize_with = "exact_f64")]
    cost: f64,
}

/// TS `SerializedSessionInfo`, in the TS key order, then this crate's
/// additions.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoOut<'a> {
    id: &'a str,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<State<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<Model<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_path: Option<&'a str>,
    rlm_depth: u32,
    message_count: usize,
    first_message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    created: &'a str,
    modified: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InfoIn {
    id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    state: Option<State<String>>,
    #[serde(default)]
    model: Option<Model<String>>,
    #[serde(default)]
    parent_session_path: Option<String>,
    rlm_depth: u32,
    message_count: usize,
    first_message: String,
    #[serde(default)]
    usage: Option<Usage>,
    created: String,
    modified: String,
    #[serde(default)]
    thinking_level: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetadataLineOut<'a> {
    file: &'a str,
    size: u64,
    #[serde(serialize_with = "js_number")]
    mtime_ms: f64,
    info: Option<InfoOut<'a>>,
    fold_version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataLineIn {
    file: String,
    size: u64,
    #[serde(deserialize_with = "exact_f64")]
    mtime_ms: f64,
    info: Option<InfoIn>,
    #[serde(default)]
    fold_version: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchLineOut<'a> {
    file: &'a str,
    size: u64,
    #[serde(serialize_with = "js_number")]
    mtime_ms: f64,
    search_text: &'a str,
    fold_version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchLineIn {
    file: String,
    size: u64,
    #[serde(deserialize_with = "exact_f64")]
    mtime_ms: f64,
    search_text: String,
    #[serde(default)]
    fold_version: Option<u32>,
}

/// Both tiers' file contents for `entries` (file name, entry), in order.
pub(crate) fn render<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a IndexedEntry)>,
) -> (String, String) {
    let header = serde_json::to_string(&Header {
        version: FORMAT_VERSION,
    })
    .expect("header serializes");
    let mut metadata = format!("{header}\n");
    let mut search = format!("{header}\n");
    for (file, indexed) in entries {
        let row = match &indexed.entry {
            CatalogEntry::Session(row) => Some(row.as_ref()),
            CatalogEntry::NotASession => None,
        };
        let line = MetadataLineOut {
            file,
            size: indexed.size,
            mtime_ms: indexed.mtime_ms,
            info: row.map(info_out),
            fold_version: indexed.fold_version,
        };
        metadata.push_str(&serde_json::to_string(&line).expect("index line serializes"));
        metadata.push('\n');
        if let Some(row) = row {
            let line = SearchLineOut {
                file,
                size: indexed.size,
                mtime_ms: indexed.mtime_ms,
                search_text: &row.all_messages_text,
                fold_version: indexed.fold_version,
            };
            search.push_str(&serde_json::to_string(&line).expect("search line serializes"));
            search.push('\n');
        }
    }
    (metadata, search)
}

fn info_out(row: &CatalogSessionRow) -> InfoOut<'_> {
    InfoOut {
        id: &row.id,
        cwd: &row.cwd,
        name: row.name.as_deref(),
        state: row.state.as_deref().map(|status| State { status }),
        model: row.model.as_ref().map(|(provider, model_id)| Model {
            provider: provider.as_str(),
            model_id: model_id.as_str(),
        }),
        parent_session_path: row.parent_session_path.as_deref(),
        rlm_depth: row.rlm_depth,
        message_count: row.message_count,
        first_message: &row.first_message,
        usage: row.usage.as_ref().map(|usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost: usage.cost,
        }),
        created: &row.created,
        modified: &row.modified,
        thinking_level: row.thinking_level.as_deref(),
    }
}

fn row_in(info: InfoIn, all_messages_text: String) -> CatalogSessionRow {
    let InfoIn {
        id,
        cwd,
        name,
        state,
        model,
        parent_session_path,
        rlm_depth,
        message_count,
        first_message,
        usage,
        created,
        modified,
        thinking_level,
    } = info;
    CatalogSessionRow {
        id,
        cwd,
        name,
        state: state.map(|state| state.status),
        model: model.map(|model| (model.provider, model.model_id)),
        thinking_level,
        parent_session_path,
        rlm_depth,
        created,
        modified,
        message_count,
        first_message,
        all_messages_text,
        usage: usage.map(|usage| CatalogUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost: usage.cost,
        }),
    }
}

/// The lines after a `{"version":2}` header, or none for a missing,
/// foreign, or outdated file. Blank lines are skipped (TS `parseLine`).
fn body_lines(contents: &str) -> impl Iterator<Item = &str> {
    let mut lines = contents.split('\n');
    let current = lines
        .next()
        .and_then(|line| serde_json::from_str::<Header>(line.trim()).ok())
        .is_some_and(|header| header.version == FORMAT_VERSION);
    lines
        .filter(move |_| current)
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// The servable entries of both tiers, keyed by file name. A torn or
/// unparseable line costs only its own file; a session row is servable only
/// with its corpus line at the same key and fold version (the catalog row
/// carries the corpus).
pub(crate) fn parse(metadata: &str, search: &str) -> HashMap<String, IndexedEntry> {
    let mut corpora: HashMap<String, SearchLineIn> = body_lines(search)
        .filter_map(|line| serde_json::from_str::<SearchLineIn>(line).ok())
        .filter(|line| line.fold_version.is_some())
        .map(|line| (line.file.clone(), line))
        .collect();
    let mut entries = HashMap::new();
    for line in body_lines(metadata) {
        let Ok(line) = serde_json::from_str::<MetadataLineIn>(line) else {
            continue;
        };
        let Some(fold_version) = line.fold_version else {
            continue;
        };
        let entry = match line.info {
            None => CatalogEntry::NotASession,
            Some(info) => {
                let Some(corpus) = corpora.remove(&line.file).filter(|corpus| {
                    corpus.size == line.size
                        && corpus.mtime_ms.to_bits() == line.mtime_ms.to_bits()
                        && corpus.fold_version == Some(fold_version)
                }) else {
                    continue;
                };
                CatalogEntry::Session(Box::new(row_in(info, corpus.search_text)))
            }
        };
        entries.insert(
            line.file,
            IndexedEntry {
                size: line.size,
                mtime_ms: line.mtime_ms,
                fold_version,
                entry,
            },
        );
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> CatalogSessionRow {
        CatalogSessionRow {
            id: "019a0000-0000-7000-8000-000000000001".to_string(),
            cwd: "/work/repo".to_string(),
            name: Some("alpha".to_string()),
            state: Some("active".to_string()),
            model: Some(("anthropic".to_string(), "claude-opus-4-5".to_string())),
            thinking_level: Some("high".to_string()),
            parent_session_path: None,
            rlm_depth: 0,
            created: "2026-09-01T10:00:00.000Z".to_string(),
            modified: "2026-09-01T10:05:00.000Z".to_string(),
            message_count: 4,
            first_message: "fix the bug".to_string(),
            all_messages_text: "fix the bug done".to_string(),
            usage: Some(CatalogUsage {
                input_tokens: 1_200,
                output_tokens: 80,
                cost: 0.25,
            }),
        }
    }

    fn indexed(entry: CatalogEntry) -> IndexedEntry {
        IndexedEntry {
            size: 2_048,
            mtime_ms: 1_789_177_896_715.123_5,
            fold_version: 1,
            entry,
        }
    }

    #[test]
    fn rendered_tiers_parse_back_to_the_same_entries() {
        let session = indexed(CatalogEntry::Session(Box::new(row())));
        let foreign = IndexedEntry {
            mtime_ms: 1_789_177_000_000.0,
            ..indexed(CatalogEntry::NotASession)
        };
        let (metadata, search) = render([("a.jsonl", &session), ("b.jsonl", &foreign)]);
        let parsed = parse(&metadata, &search);
        let expected = HashMap::from([
            ("a.jsonl".to_string(), session),
            ("b.jsonl".to_string(), foreign),
        ]);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn floats_survive_the_round_trip_bit_for_bit() {
        // serde_json's default float parse may land one ULP off these.
        let mut row = row();
        row.usage = Some(CatalogUsage {
            input_tokens: 1,
            output_tokens: 2,
            cost: 38.469_787_499_999_924,
        });
        let mut entries = Vec::new();
        for (index, (cost, mtime_ms)) in [
            (38.469_787_499_999_924, 1_789_177_896_715.123_5),
            (1.300_425_200_000_000_3, 1_759_000_000_123.456_7),
            (22.942_794_499_999_998, 1_759_000_000_000.000_2),
            (3.212_507_400_000_000_2, 0.1 + 0.2),
        ]
        .into_iter()
        .enumerate()
        {
            let mut row = row.clone();
            row.usage = Some(CatalogUsage {
                input_tokens: 1,
                output_tokens: 2,
                cost,
            });
            let entry = IndexedEntry {
                mtime_ms,
                ..indexed(CatalogEntry::Session(Box::new(row)))
            };
            entries.push((format!("{index}.jsonl"), entry));
        }
        let (metadata, search) = render(entries.iter().map(|(file, entry)| (file.as_str(), entry)));
        assert_eq!(parse(&metadata, &search), entries.into_iter().collect());
    }

    #[test]
    fn whole_numbers_print_like_json_stringify() {
        let mut row = row();
        row.usage = Some(CatalogUsage {
            input_tokens: 1,
            output_tokens: 2,
            cost: 0.0,
        });
        row.thinking_level = None;
        let entry = IndexedEntry {
            mtime_ms: 1_789_177_000_000.0,
            ..indexed(CatalogEntry::Session(Box::new(row)))
        };
        let (metadata, _) = render([("a.jsonl", &entry)]);
        let line = metadata.lines().nth(1).unwrap();
        assert!(line.contains("\"mtimeMs\":1789177000000,"), "{line}");
        assert!(line.contains("\"cost\":0}"), "{line}");
    }

    #[test]
    fn a_session_row_without_its_corpus_line_is_not_served() {
        let session = indexed(CatalogEntry::Session(Box::new(row())));
        let (metadata, _) = render([("a.jsonl", &session)]);
        assert_eq!(parse(&metadata, ""), HashMap::new());
        // A corpus line recorded at another key does not complete the row.
        let stale = IndexedEntry {
            size: 1_024,
            ..session
        };
        let (_, stale_search) = render([("a.jsonl", &stale)]);
        assert_eq!(parse(&metadata, &stale_search), HashMap::new());
    }

    #[test]
    fn a_torn_line_costs_only_its_own_file() {
        let first = indexed(CatalogEntry::Session(Box::new(row())));
        let second = indexed(CatalogEntry::NotASession);
        let (metadata, search) = render([("a.jsonl", &first), ("b.jsonl", &second)]);
        let torn = metadata.trim_end().rsplit_once('\n').unwrap().0;
        let torn = format!("{torn}\n{{\"file\":\"b.jsonl\",\"si");
        let parsed = parse(&torn, &search);
        assert_eq!(parsed, HashMap::from([("a.jsonl".to_string(), first)]));
    }

    #[test]
    fn another_format_version_yields_nothing() {
        let session = indexed(CatalogEntry::Session(Box::new(row())));
        let (metadata, search) = render([("a.jsonl", &session)]);
        let metadata = metadata.replacen("{\"version\":2}", "{\"version\":1}", 1);
        assert_eq!(parse(&metadata, &search), HashMap::new());
    }
}
