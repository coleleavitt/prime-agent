//! The harness-render seam: optional features adjust how the merged harness
//! state renders into the model-facing digest, without native code naming
//! them (see `docs/fork-feature-crates.md`).
//!
//! A [`HarnessPromptHook`] looks at the merged state about to render and
//! answers a [`HarnessPromptAdjustment`]: a lead sort key per entry (lower
//! renders first, ahead of the native relevance/path order, so an entry can
//! win or lose a rendered slot), entries withheld from the listing (counted
//! per kind on one announcement line, so the model knows they exist), and
//! extra sections rendered after the entries. With no hook, or an empty
//! adjustment, the digest is byte-identical to the native one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Serialize;

use super::{HarnessState, RefinementKind};

/// Entries one feature withholds from the listing, announced per kind as
/// `- +<n> <label> <kind> entries (<note>)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WithheldHarnessEntries {
    /// One word naming why (`dormant`, ...).
    pub label: String,
    /// The parenthesized explanation on the announcement line.
    pub note: String,
    /// The withheld entries: kind and merged entry id.
    pub entries: BTreeSet<(RefinementKind, String)>,
}

/// A section rendered after the entries, before the refinement history:
/// the heading line, then each non-empty line (sanitized to one line and
/// bounded like entry content) as a `- ` bullet, then a blank line. A
/// section with no non-empty line is not rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HarnessPromptSection {
    pub heading: String,
    pub lines: Vec<String>,
}

/// What hooks change about one render of the harness state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HarnessPromptAdjustment {
    /// Lead sort key per merged entry id (in every kind); an unlisted entry
    /// ranks 0. Lower renders earlier; ties keep the native order.
    pub entry_rank: BTreeMap<String, i64>,
    /// Entries withheld from the listing.
    pub withheld: Vec<WithheldHarnessEntries>,
    /// Sections rendered after the entries.
    pub sections: Vec<HarnessPromptSection>,
}

impl HarnessPromptAdjustment {
    /// Whether the adjustment changes nothing about a render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entry_rank.values().all(|rank| *rank == 0)
            && self.withheld.iter().all(|group| group.entries.is_empty())
            && self.sections.iter().all(|section| {
                section
                    .lines
                    .iter()
                    .all(|line| sanitize_prompt_line(line, usize::MAX).is_empty())
            })
    }

    /// Fold `other` into `self`: ranks add up, withheld groups and sections
    /// append in order.
    pub fn merge(&mut self, other: HarnessPromptAdjustment) {
        for (id, rank) in other.entry_rank {
            *self.entry_rank.entry(id).or_insert(0) += rank;
        }
        self.withheld.extend(other.withheld);
        self.sections.extend(other.sections);
    }

    /// The lead sort key of the entry with merged id `id`.
    #[must_use]
    pub fn rank(&self, id: &str) -> i64 {
        self.entry_rank.get(id).copied().unwrap_or(0)
    }

    /// The first withheld group naming the entry, if any.
    #[must_use]
    pub fn withheld_group(&self, kind: RefinementKind, id: &str) -> Option<usize> {
        let key = (kind, id.to_string());
        self.withheld
            .iter()
            .position(|group| group.entries.contains(&key))
    }
}

/// A feature's adjustment of the harness digest. Called on the digest's
/// render path (session start, resume, compaction, every staleness check),
/// so it must stay cheap: no network, at most a small cached file read.
pub trait HarnessPromptHook: Send + Sync {
    /// The adjustment for one render of `state` (the merged global + local
    /// state the digest is about to render).
    fn adjust(&self, state: &HarnessState) -> HarnessPromptAdjustment;
}

/// The hooks one session renders its digest with, in feature order.
#[derive(Clone, Default)]
pub struct HarnessPromptHooks(pub Vec<Arc<dyn HarnessPromptHook>>);

impl std::fmt::Debug for HarnessPromptHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HarnessPromptHooks({})", self.0.len())
    }
}

impl HarnessPromptHooks {
    /// The combined adjustment for one render of `state`; `None` when no
    /// hook is installed or none changes anything.
    #[must_use]
    pub fn adjust(&self, state: &HarnessState) -> Option<HarnessPromptAdjustment> {
        let mut adjustment = HarnessPromptAdjustment::default();
        for hook in &self.0 {
            adjustment.merge(hook.adjust(state));
        }
        (!adjustment.is_empty()).then_some(adjustment)
    }
}

/// One untrusted line made safe for the prompt: every whitespace run is one
/// space, control/format/private-use characters are dropped, the result is
/// trimmed, `<`/`>` are escaped, and it is cut to `max_chars` code points
/// (with `...`).
#[must_use]
pub fn sanitize_prompt_line(value: &str, max_chars: usize) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut in_space = false;
    for ch in value.chars() {
        // ECMAScript `\s`: Unicode White_Space without NEL, plus BOM.
        if (ch.is_whitespace() && ch != '\u{0085}') || ch == '\u{FEFF}' {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if is_dropped_char(ch) {
            continue;
        }
        collapsed.push(ch);
    }
    // Dropping a character can join two spaces.
    let mut cleaned = String::with_capacity(collapsed.len());
    for ch in collapsed.chars() {
        if ch == ' ' && cleaned.ends_with(' ') {
            continue;
        }
        cleaned.push(ch);
    }
    let escaped = cleaned
        .trim_matches(|ch: char| ch.is_whitespace())
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let count = escaped.chars().count();
    if count <= max_chars {
        return escaped;
    }
    let mut cut: String = escaped.chars().take(max_chars.saturating_sub(3)).collect();
    cut.push_str("...");
    cut
}

/// Unicode `Cc`, `Cf`, `Co` (and lone surrogates, which a Rust string
/// cannot hold).
fn is_dropped_char(ch: char) -> bool {
    let code = u32::from(ch);
    ch.is_control()
        || matches!(code,
            0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x1_10BD
            | 0x1_10CD
            | 0x1_3430..=0x1_343F
            | 0x1_BCA0..=0x1_BCA3
            | 0x1_D173..=0x1_D17A
            | 0xE_0001
            | 0xE_0020..=0xE_007F
            | 0xE000..=0xF8FF
            | 0xF_0000..=0xF_FFFD
            | 0x10_0000..=0x10_FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_escaped_bounded_line() {
        assert_eq!(
            sanitize_prompt_line("  a\nb <script>x</script>\u{200B}  c\t ", usize::MAX),
            "a b &lt;script&gt;x&lt;/script&gt; c"
        );
        assert_eq!(sanitize_prompt_line("abcdefgh", 6), "abc...");
        assert_eq!(
            sanitize_prompt_line("\u{1F600}\u{1F600}x", 3),
            "\u{1F600}\u{1F600}x"
        );
        assert_eq!(sanitize_prompt_line(" \u{0007} ", 10), "");
        // NEL is a control to ECMAScript, not whitespace.
        assert_eq!(sanitize_prompt_line("a\u{0085}b", 10), "ab");
    }

    #[test]
    fn adjustments_merge_and_an_inert_one_is_empty() {
        let mut adjustment = HarnessPromptAdjustment::default();
        assert!(adjustment.is_empty());
        adjustment.merge(HarnessPromptAdjustment {
            entry_rank: BTreeMap::from([("a".to_string(), 0)]),
            withheld: vec![WithheldHarnessEntries::default()],
            sections: vec![HarnessPromptSection {
                heading: "h".to_string(),
                lines: vec![" \n ".to_string()],
            }],
        });
        assert!(adjustment.is_empty());
        adjustment.merge(HarnessPromptAdjustment {
            entry_rank: BTreeMap::from([("a".to_string(), -2), ("b".to_string(), 3)]),
            ..HarnessPromptAdjustment::default()
        });
        assert!(!adjustment.is_empty());
        assert_eq!(
            (
                adjustment.rank("a"),
                adjustment.rank("b"),
                adjustment.rank("c")
            ),
            (-2, 3, 0)
        );
    }

    fn memory(id: &str, path: &str) -> super::super::HarnessEntry {
        super::super::HarnessEntry {
            id: id.to_string(),
            kind: RefinementKind::Memory,
            title: format!("note {id}"),
            content: format!("content {id}"),
            path: path.to_string(),
            scope: Some(super::super::HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
        }
    }

    fn four_memories() -> HarnessState {
        let mut state = super::super::empty_harness_state();
        let memories = state.entries.get_mut(&RefinementKind::Memory).unwrap();
        for (index, id) in ["a", "b", "c", "d"].iter().enumerate() {
            memories.insert((*id).to_string(), memory(id, &format!("m/{index}")));
        }
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

    fn render(state: &HarnessState, hooks: &HarnessPromptHooks) -> String {
        let adjustment = hooks.adjust(state);
        super::super::ranking::format_harness_state_for_prompt(
            state,
            &super::super::ranking::HarnessStatePromptOptions {
                adjustment,
                ..Default::default()
            },
        )
    }

    /// A stub feature's hook: answers a fixed adjustment.
    struct Fixed(HarnessPromptAdjustment);

    impl HarnessPromptHook for Fixed {
        fn adjust(&self, _state: &HarnessState) -> HarnessPromptAdjustment {
            self.0.clone()
        }
    }

    fn hooks(adjustments: Vec<HarnessPromptAdjustment>) -> HarnessPromptHooks {
        HarnessPromptHooks(
            adjustments
                .into_iter()
                .map(|adjustment| Arc::new(Fixed(adjustment)) as Arc<dyn HarnessPromptHook>)
                .collect(),
        )
    }

    #[test]
    fn no_hook_and_an_inert_hook_render_the_native_digest() {
        let state = four_memories();
        let native = render(&state, &HarnessPromptHooks::default());
        assert_eq!(rendered_ids(&native), ["a", "b", "c"]);
        assert!(native.contains("- +1 more memory entries"), "{native}");
        let inert = hooks(vec![HarnessPromptAdjustment {
            entry_rank: BTreeMap::from([("a".to_string(), 0)]),
            withheld: vec![WithheldHarnessEntries {
                label: "dormant".to_string(),
                note: "n".to_string(),
                entries: BTreeSet::new(),
            }],
            sections: vec![HarnessPromptSection {
                heading: "h:".to_string(),
                lines: vec!["\n".to_string()],
            }],
        }]);
        assert_eq!(render(&state, &inert), native);
    }

    #[test]
    fn ranks_lead_the_native_order_and_move_entries_across_the_slice() {
        let state = four_memories();
        let ranked = hooks(vec![HarnessPromptAdjustment {
            entry_rank: BTreeMap::from([("d".to_string(), -1), ("a".to_string(), 1)]),
            ..HarnessPromptAdjustment::default()
        }]);
        assert_eq!(rendered_ids(&render(&state, &ranked)), ["d", "b", "c"]);
    }

    #[test]
    fn withheld_entries_are_counted_not_listed_and_sections_render_sanitized() {
        let state = four_memories();
        let prompt = render(
            &state,
            &hooks(vec![
                HarnessPromptAdjustment {
                    withheld: vec![WithheldHarnessEntries {
                        label: "dormant".to_string(),
                        note: "below trust threshold; still readable and editable".to_string(),
                        entries: BTreeSet::from([
                            (RefinementKind::Memory, "a".to_string()),
                            (RefinementKind::Memory, "b".to_string()),
                            // Another kind's id never withholds a memory.
                            (RefinementKind::Skill, "c".to_string()),
                        ]),
                    }],
                    ..HarnessPromptAdjustment::default()
                },
                HarnessPromptAdjustment {
                    sections: vec![HarnessPromptSection {
                        heading: "extra (stub):".to_string(),
                        lines: vec![
                            "one <b>\nline".to_string(),
                            "  ".to_string(),
                            "two".to_string(),
                        ],
                    }],
                    ..HarnessPromptAdjustment::default()
                },
            ]),
        );
        let lines: Vec<&str> = prompt.lines().collect();
        let memory_at = lines
            .iter()
            .position(|line| *line == "memory: 2")
            .expect("count");
        assert_eq!(
            lines[memory_at..memory_at + 5],
            [
                "memory: 2",
                "- [global:c] note c (m/2, v1): content c",
                "- [global:d] note d (m/3, v1): content d",
                "- +2 dormant memory entries (below trust threshold; still readable and editable)",
                "",
            ]
        );
        let section_at = lines
            .iter()
            .position(|line| *line == "extra (stub):")
            .expect("section");
        assert_eq!(
            lines[section_at..section_at + 5],
            [
                "extra (stub):",
                "- one &lt;b&gt; line",
                "- two",
                "",
                "recent refinements: 0"
            ]
        );
    }
}
