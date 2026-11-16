//! Supersession: which memories a newer one replaced. Explicit records win
//! (the user's or model's `brain.supersede`, an entry's own
//! `metadata.supersededBy`); then two mechanical rules for legacy data, both
//! keeping the newest: a `<stem>_part_<N>` handoff series keeps its highest
//! part, and identical normalized content keeps one copy.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

/// Why a memory counts as superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Reason {
    /// A `brain.supersede` record.
    Recorded,
    /// The memory's own `metadata.supersededBy`.
    Declared,
    /// An older part of a `<stem>_part_<N>` series.
    Series,
    /// The same content as a newer memory.
    Duplicate,
}

/// One memory's supersession.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Superseded {
    /// The key of the memory that replaces it.
    pub(crate) by: String,
    pub(crate) reason: Reason,
}

/// What the rules read of one memory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Item<'a> {
    pub(crate) key: &'a str,
    /// The name a series is spotted in (the harness id or the file stem).
    pub(crate) name: &'a str,
    pub(crate) content: &'a str,
    pub(crate) updated_at: &'a str,
    pub(crate) declared: Option<&'a str>,
}

/// The shortest normalized content the duplicate rule considers: shorter
/// texts ("done", "n/a") collide by accident.
const MIN_DUPLICATE_CHARS: usize = 24;

/// The superseded memories among `items`, keyed by key.
pub(crate) fn superseded(
    items: &[Item<'_>],
    recorded: &BTreeMap<String, String>,
) -> BTreeMap<String, Superseded> {
    let mut out = BTreeMap::new();
    for item in items {
        if let Some(by) = recorded.get(item.key) {
            out.insert(
                item.key.to_string(),
                Superseded {
                    by: by.clone(),
                    reason: Reason::Recorded,
                },
            );
        } else if let Some(by) = item.declared.filter(|by| *by != item.key) {
            out.insert(
                item.key.to_string(),
                Superseded {
                    by: by.to_string(),
                    reason: Reason::Declared,
                },
            );
        }
    }

    let mut series: HashMap<&str, Vec<(u64, &Item<'_>)>> = HashMap::new();
    for item in items {
        if let Some((stem, part)) = series_part(item.name) {
            series.entry(stem).or_default().push((part, item));
        }
    }
    for members in series.values().filter(|members| members.len() > 1) {
        let newest = members
            .iter()
            .max_by(|a, b| (a.0, a.1.updated_at, a.1.key).cmp(&(b.0, b.1.updated_at, b.1.key)))
            .map(|(_, item)| item.key);
        if let Some(newest) = newest {
            mark_all(
                &mut out,
                members.iter().map(|(_, item)| *item),
                newest,
                Reason::Series,
            );
        }
    }

    let mut copies: HashMap<String, Vec<&Item<'_>>> = HashMap::new();
    for item in items {
        let normalized = normalize(item.content);
        if normalized.chars().count() >= MIN_DUPLICATE_CHARS {
            copies.entry(normalized).or_default().push(item);
        }
    }
    for members in copies.values().filter(|members| members.len() > 1) {
        let newest = members
            .iter()
            .max_by(|a, b| (a.updated_at, a.key).cmp(&(b.updated_at, b.key)))
            .map(|item| item.key);
        if let Some(newest) = newest {
            mark_all(&mut out, members.iter().copied(), newest, Reason::Duplicate);
        }
    }

    // A record naming a vanished memory is still honoured (the user said
    // so); only self-references are dropped.
    out.retain(|key, superseded| superseded.by != *key);
    out
}

fn mark_all<'a>(
    out: &mut BTreeMap<String, Superseded>,
    members: impl Iterator<Item = &'a Item<'a>>,
    newest: &str,
    reason: Reason,
) {
    for member in members {
        if member.key != newest {
            out.entry(member.key.to_string())
                .or_insert_with(|| Superseded {
                    by: newest.to_string(),
                    reason,
                });
        }
    }
}

/// `(stem, N)` of a `<stem>_part_<N>[_<rest>]` / `<stem>-part-<N>[-<rest>]`
/// name (live handoff ids carry a description after the part number:
/// `..._review_part_37_all_deploys_done`).
fn series_part(name: &str) -> Option<(&str, u64)> {
    const MARKER_LEN: usize = "_part_".len();
    let at = name.rfind("_part_").or_else(|| name.rfind("-part-"))?;
    let rest = &name[at + MARKER_LEN..];
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let boundary_ok = rest[digits..].is_empty() || rest[digits..].starts_with(['_', '-']);
    let part = rest[..digits].parse().ok().filter(|_| boundary_ok)?;
    let stem = &name[..at];
    (!stem.is_empty()).then_some((stem, part))
}

/// Case-folded, whitespace-collapsed content.
fn normalize(content: &str) -> String {
    content
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>(key: &'a str, name: &'a str, content: &'a str, updated_at: &'a str) -> Item<'a> {
        Item {
            key,
            name,
            content,
            updated_at,
            declared: None,
        }
    }

    #[test]
    fn a_handoff_series_keeps_only_its_highest_part() {
        let items = [
            item("global:h_part_2", "h_part_2", "two", "2026-11-02"),
            item("global:h_part_10", "h_part_10", "ten", "2026-11-01"),
            item("session:s:h_part_3", "h_part_3", "three", "2026-11-03"),
            item("global:other", "other", "unrelated", "2026-11-01"),
        ];
        let out = superseded(&items, &BTreeMap::new());
        assert_eq!(out.len(), 2);
        assert_eq!(
            out["global:h_part_2"],
            Superseded {
                by: "global:h_part_10".to_string(),
                reason: Reason::Series
            }
        );
        assert_eq!(out["session:s:h_part_3"].by, "global:h_part_10");
    }

    #[test]
    fn identical_content_keeps_the_newest_copy() {
        let text = "Build with mold as the linker and sccache as the wrapper";
        let items = [
            item("global:a", "a", text, "2026-11-01"),
            item(
                "session:s:b",
                "b",
                "build  with MOLD as the linker and sccache as the wrapper",
                "2026-11-05",
            ),
            item("global:short1", "short1", "done", "2026-11-01"),
            item("global:short2", "short2", "done", "2026-11-02"),
        ];
        let out = superseded(&items, &BTreeMap::new());
        assert_eq!(
            out,
            BTreeMap::from([(
                "global:a".to_string(),
                Superseded {
                    by: "session:s:b".to_string(),
                    reason: Reason::Duplicate
                }
            )])
        );
    }

    #[test]
    fn explicit_records_win_over_mechanical_rules() {
        let mut declared = item("global:x_part_1", "x_part_1", "one", "2026-11-01");
        declared.declared = Some("global:y");
        let items = [
            declared,
            item("global:x_part_2", "x_part_2", "two", "2026-11-02"),
            item("global:z", "z", "zed", "2026-11-03"),
        ];
        let recorded = BTreeMap::from([("global:z".to_string(), "global:x_part_2".to_string())]);
        let out = superseded(&items, &recorded);
        assert_eq!(out["global:x_part_1"].reason, Reason::Declared);
        assert_eq!(out["global:x_part_1"].by, "global:y");
        assert_eq!(out["global:z"].reason, Reason::Recorded);
        assert!(!out.contains_key("global:x_part_2"));
    }

    #[test]
    fn series_names_parse_both_separators() {
        assert_eq!(series_part("a_b_part_12"), Some(("a_b", 12)));
        assert_eq!(series_part("notes-part-3"), Some(("notes", 3)));
        assert_eq!(
            series_part("review_part_37_all_deploys_done"),
            Some(("review", 37))
        );
        assert_eq!(series_part("_part_3"), None);
        assert_eq!(series_part("part_x"), None);
        assert_eq!(series_part("x_part_3rd"), None);
    }
}
