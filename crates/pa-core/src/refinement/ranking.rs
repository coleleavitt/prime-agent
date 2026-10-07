//! Harness relevance ranking and prompt rendering (query terms, scoring,
//! digest formatting).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::{
    compact_harness_text, HarnessEntry, HarnessState, RefinementKind,
    DEFAULT_OVERVIEW_CONTENT_LIMIT, DEFAULT_OVERVIEW_ENTRY_LIMIT,
    DEFAULT_OVERVIEW_REFINEMENT_LIMIT, REFINEMENT_KINDS,
};

/// Term -> weight, built from task signal (goal, recent messages).
pub type HarnessQueryTerms = HashMap<String, f64>;

fn is_cjk(char: char) -> bool {
    matches!(char as u32,
        0x3040..=0x30ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xf900..=0xfaff
        | 0xac00..=0xd7af
        | 0x20000..=0x2a6df
        | 0x2a700..=0x2b73f
        | 0x2b740..=0x2b81f
        | 0x2b820..=0x2ceaf
        | 0x2ceb0..=0x2ebef
        | 0x2ebf0..=0x2ee5f
        | 0x2f800..=0x2fa1f
        | 0x30000..=0x3134f
        | 0x31350..=0x323af
        | 0x323b0..=0x3347f)
}

/// Tokenize text into lowercase query terms: word runs of letters/digits/marks
/// (>= 4 chars), CJK runs as overlapping bigrams.
///
/// # Panics
///
/// The `next().unwrap()` cannot fire: the run is checked non-empty first.
#[must_use]
pub fn harness_query_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut run = String::new();
    let flush_run = |run: &mut String, terms: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        let mut segment = String::new();
        let mut segment_is_cjk = is_cjk(run.chars().next().unwrap());
        for char in run.chars() {
            if is_cjk(char) == segment_is_cjk {
                segment.push(char);
            } else {
                push_segment(&segment, segment_is_cjk, terms);
                segment.clear();
                segment.push(char);
                segment_is_cjk = !segment_is_cjk;
            }
        }
        push_segment(&segment, segment_is_cjk, terms);
        run.clear();
    };
    for char in text.to_lowercase().chars() {
        if char.is_alphabetic() || char.is_numeric() || char.is_alphanumeric() {
            run.push(char);
        } else {
            flush_run(&mut run, &mut terms);
        }
    }
    flush_run(&mut run, &mut terms);
    // Distinct terms, order preserved.
    let mut seen: Vec<String> = Vec::new();
    terms.retain(|term| {
        if seen.contains(term) {
            false
        } else {
            seen.push(term.clone());
            true
        }
    });
    terms
}

fn push_segment(segment: &str, is_cjk_segment: bool, terms: &mut Vec<String>) {
    if segment.is_empty() {
        return;
    }
    if is_cjk_segment {
        let chars: Vec<char> = segment.chars().collect();
        if chars.len() == 1 {
            terms.push(segment.to_string());
        } else {
            for window in chars.windows(2) {
                terms.push(window.iter().collect());
            }
        }
    } else if segment.chars().count() >= 4 {
        terms.push(segment.to_string());
    }
}

/// Inverse document frequency per query term over the entries being ranked:
/// `ln(1 + documents / matches)` — rare distinctive terms outrank ubiquitous
/// ones; terms matching no entry are absent (they cannot score anything).
#[must_use]
pub fn harness_query_term_idf(
    entries: &[HarnessEntry],
    terms: &HarnessQueryTerms,
) -> HarnessQueryTerms {
    let mut idf = HarnessQueryTerms::new();
    if terms.is_empty() {
        return idf;
    }
    let mut matches: HashMap<&str, usize> = HashMap::new();
    for entry in entries {
        let title = entry.title.to_lowercase();
        let content = entry.content.to_lowercase();
        let identifier = format!("{} {}", entry.path.to_lowercase(), entry.id.to_lowercase());
        for term in terms.keys() {
            if title.contains(term.as_str())
                || content.contains(term.as_str())
                || identifier.contains(term.as_str())
            {
                *matches.entry(term.as_str()).or_insert(0) += 1;
            }
        }
    }
    for (term, document_frequency) in matches {
        idf.insert(
            term.to_string(),
            (1.0 + entries.len() as f64 / document_frequency as f64).ln(),
        );
    }
    idf
}

/// Score one entry against query terms: weighted per-term overlap across
/// title/content/identifier fields (coverage weighted, not repetition),
/// each term discounted by its document frequency in the ranked corpus
/// (`idf`; a missing map weights every term at 1).
#[must_use]
pub fn score_harness_entry_for_query(
    entry: &HarnessEntry,
    terms: &HarnessQueryTerms,
    idf: Option<&HarnessQueryTerms>,
) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let title = entry.title.to_lowercase();
    let content = entry.content.to_lowercase();
    let identifier = format!("{} {}", entry.path.to_lowercase(), entry.id.to_lowercase());
    let mut score = 0.0;
    for (term, weight) in terms {
        let mut fields = 0;
        if title.contains(term.as_str()) {
            fields += 1;
        }
        if content.contains(term.as_str()) {
            fields += 1;
        }
        if identifier.contains(term.as_str()) {
            fields += 1;
        }
        if fields > 0 {
            let term_idf = idf
                .and_then(|map| map.get(term.as_str()).copied())
                .unwrap_or(1.0);
            score += weight * term_idf * (1.0 + f64::from(fields - 1) * 0.5);
        }
    }
    score
}

fn entry_sort_key(entry: &HarnessEntry) -> String {
    format!("{}\0{}\0{}", entry.path, entry.title, entry.id)
}

/// The native digest order: relevance to the query terms when there are
/// any (ties by path, title, id), else path, title, id.
fn native_entry_order(
    a: &HarnessEntry,
    b: &HarnessEntry,
    query_terms: Option<&HarnessQueryTerms>,
    idf: Option<&HarnessQueryTerms>,
) -> std::cmp::Ordering {
    match query_terms {
        Some(terms) if !terms.is_empty() => {
            let by_score = score_harness_entry_for_query(b, terms, idf)
                .partial_cmp(&score_harness_entry_for_query(a, terms, idf))
                .unwrap_or(std::cmp::Ordering::Equal);
            if by_score != std::cmp::Ordering::Equal {
                return by_score;
            }
            entry_sort_key(a).cmp(&entry_sort_key(b))
        }
        _ => entry_sort_key(a).cmp(&entry_sort_key(b)),
    }
}

#[derive(Debug, Default)]
pub struct HarnessStatePromptOptions {
    pub max_entries_per_kind: Option<usize>,
    pub max_refinements: Option<usize>,
    pub max_content_length: Option<usize>,
    pub include_ipython_examples: Option<bool>,
    pub include_shell_examples: bool,
    pub include_refine_examples: Option<bool>,
    pub query_terms: Option<HarnessQueryTerms>,
    /// What installed features change about this render
    /// ([`super::prompt_hook`]); `None` (or an empty adjustment) renders the
    /// native digest byte for byte.
    pub adjustment: Option<super::prompt_hook::HarnessPromptAdjustment>,
}

/// Render the harness state as the model-facing digest block. Strings must
/// stay byte-identical with the TS formatter.
#[must_use]
pub fn format_harness_state_for_prompt(
    state: &HarnessState,
    options: &HarnessStatePromptOptions,
) -> String {
    let max_entries_per_kind = options
        .max_entries_per_kind
        .unwrap_or(DEFAULT_OVERVIEW_ENTRY_LIMIT);
    let max_refinements = options
        .max_refinements
        .unwrap_or(DEFAULT_OVERVIEW_REFINEMENT_LIMIT);
    let max_content_length = options
        .max_content_length
        .unwrap_or(DEFAULT_OVERVIEW_CONTENT_LIMIT);
    let include_ipython = options.include_ipython_examples.unwrap_or(true);
    let include_refine = options.include_refine_examples.unwrap_or(include_ipython);

    let mut lines: Vec<String> = vec![
        "# Continual Harness State".to_string(),
        String::new(),
        "Local continual harness entries belong to this Prime Agent session. Global continual harness entries persist across Prime Agent sessions. Package continual harness entries are read-only overlays mounted from installed Prime Agent packages; they update or disappear with the package and are never copied into editable harness state.".to_string(),
        "Never update or delete a package entry with `/refine`; create an editable local or global entry with the same kind and id to override one. Package provenance lists the configured source, install scope, and package-relative file.".to_string(),
        "The continual harness entries below are compact summaries, not full descriptions. Use them as routing/context hints; inspect or refine the underlying continual harness entry only when detail matters.".to_string(),
        "Default to local continual harness refinement for current task progress, temporary blockers, and session coordination. Use global continual harness refinement only for stable cross-session lessons, durable user preferences, reusable skills/subagents, or explicitly project-qualified facts.".to_string(),
        "Use these continual harness prompt notes, memories, skills, and subagent specs when they are relevant. The base system prompt is immutable; prompt entries below are supplemental notes only.".to_string(),
        String::new(),
        if include_refine {
            "When to call `await refine.run()`: after a repeated failure, a reusable tactic emerges, a repeated delegation role should become a subagent spec, a repeated procedure should become a skill, a durable fact/preference should become a memory, a narrow behavioral policy should become a prompt addendum, a user corrects behavior that should persist locally or globally, validation shows a continual harness entry is wrong, or a skill/subagent/memory/prompt note should be created, updated, deleted, or rolled back. Keep `await refine.run()` continual harness edits small and evidence-backed.".to_string()
        } else {
            "When to refine the continual harness: after a repeated failure, a reusable tactic emerges, a repeated delegation role should become a subagent spec, a repeated procedure should become a skill, a durable fact/preference should become a memory, a narrow behavioral policy should become a prompt addendum, a user corrects behavior that should persist locally or globally, validation shows a continual harness entry is wrong, or a skill/subagent/memory/prompt note should be created, updated, deleted, or rolled back. Keep continual harness edits small and evidence-backed.".to_string()
        },
        String::new(),
        if include_ipython {
            "Call contract: read each installed Python skill's SKILL.md and call its documented module function in the Python REPL; do not assume a `.run` entrypoint. Use `<skill_import> ...` in shell when a CLI exists. Continual harness skill entries are Python REPL skills with an explicit Python `reference` and `arguments` contract. Spawn a continual harness subagent spec by composing a concise task prompt and calling `handle = await rlm.spawn('sub-task', name='worker')`; admission returns immediately with `rlm_child_id`, `name`, `session_dir`, and `model`, never the child's answer. Results arrive only through explicit `agent_message` replies or files; children reply with `await agent_message.send(message, receiver_role='parent')`. Use `await rlm.list_subagents()` to recover direct child handles and `await agent_message.send(..., receiver_role='child', receiver_name=handle.name)` for follow-ups. Do not invent wrappers such as `call_skill(...)`, `run_subagent(...)`, or named subagent registries.".to_string()
        } else if options.include_shell_examples {
            "Call contract: use installed skills as shell commands when available (for example `<skill_import> ...`). Continual harness entries are routing/context hints only in sessions without the Python REPL; do not use Python `await`, `asyncio`, or `rlm` examples unless the prompt also documents a Python kernel.".to_string()
        } else {
            "Call contract: continual harness entries are routing/context hints only in sessions without the Python REPL or shell access; do not use Python `await`, `asyncio`, `rlm`, or shell skill commands unless the prompt also documents those interfaces.".to_string()
        },
        String::new(),
    ];

    let query_terms = &options.query_terms;
    let adjustment = options.adjustment.as_ref();
    let mut total_entries = 0usize;
    for kind in REFINEMENT_KINDS {
        let entries = state
            .entries
            .get(&kind_for(kind))
            .cloned()
            .unwrap_or_default();
        // Entries a feature withholds are counted per group, not listed.
        let mut withheld_counts: Vec<usize> =
            vec![0; adjustment.map_or(0, |adjustment| adjustment.withheld.len())];
        let entries: Vec<HarnessEntry> = entries
            .into_iter()
            .filter(|(id, _)| {
                let group =
                    adjustment.and_then(|adjustment| adjustment.withheld_group(kind_for(kind), id));
                if let Some(group) = group {
                    withheld_counts[group] += 1;
                }
                group.is_none()
            })
            .map(|(_, entry)| entry)
            .collect();
        // Disabled entries stay stored but are never advertised (#1118): a
        // disabled subagent spec must not be matched against a task.
        let all_count = entries.len();
        let mut entries: Vec<HarnessEntry> = entries
            .into_iter()
            .filter(HarnessEntry::is_enabled)
            .collect();
        let disabled_suffix = match all_count - entries.len() {
            0 => String::new(),
            disabled => format!(" (+{disabled} disabled, not available)"),
        };
        // The ranked corpus is the kind's own entries: they compete for the
        // same top-k slots, so document frequency discounts terms ubiquitous
        // within the kind rather than across unrelated kinds.
        let ranked_idf = match query_terms.as_ref() {
            Some(terms) if !terms.is_empty() => Some(harness_query_term_idf(&entries, terms)),
            _ => None,
        };
        // A feature's rank leads; the native order breaks its ties.
        let rank =
            |entry: &HarnessEntry| adjustment.map_or(0, |adjustment| adjustment.rank(&entry.id));
        // Package overlays rank after their own-kind editable competition
        // (TS: the package rank leads the sort).
        let package = |entry: &HarnessEntry| super::package_harness::is_package_entry(entry);
        entries.sort_by(|a, b| {
            package(a)
                .cmp(&package(b))
                .then_with(|| rank(a).cmp(&rank(b)))
                .then_with(|| native_entry_order(a, b, query_terms.as_ref(), ranked_idf.as_ref()))
        });
        total_entries += entries.len();
        let kind_name = kind;
        if kind_name == "subagent" && !entries.is_empty() && include_ipython {
            lines.push(format!("{kind_name}: {}{disabled_suffix} (invoke a spec by turning it into a concise task prompt and spawning with `await rlm.spawn('<task>', name='<worker>')`; admission returns a child handle, never the answer)", entries.len()));
        } else if kind_name == "factory" && !entries.is_empty() && include_ipython {
            lines.push(format!(
                "{kind_name}: {}{disabled_suffix} (state-machine workflow specs; run one with `await rlm.factory.run('<id>')`; watch with `await rlm.factory.status(run_id)`, stop with `await rlm.factory.stop(run_id)`, resume a paused run with `await rlm.factory.resume(run_id)`)",
                entries.len()
            ));
        } else {
            lines.push(format!("{kind_name}: {}{disabled_suffix}", entries.len()));
        }
        if let Some(terms) = query_terms.as_ref() {
            if !terms.is_empty() && entries.len() > max_entries_per_kind {
                lines.push(
                    "(entries ranked by relevance to the current task; see harness.search)"
                        .to_string(),
                );
            }
        }
        for entry in entries.iter().take(max_entries_per_kind) {
            let arguments_text =
                if entry.kind == RefinementKind::Skill && !entry.arguments.is_empty() {
                    format!(
                        " args={}",
                        compact_harness_text(
                            &serde_json::to_string(&entry.arguments).unwrap_or_default(),
                            max_content_length
                        )
                    )
                } else {
                    String::new()
                };
            let reference_text =
                if entry.kind == RefinementKind::Skill && !entry.reference.is_empty() {
                    format!(
                        " ref={}",
                        compact_harness_text(
                            &serde_json::to_string(&entry.reference).unwrap_or_default(),
                            max_content_length
                        )
                    )
                } else {
                    String::new()
                };
            lines.push(format!(
                "- [{}] {} ({}, v{}){}{}{}: {}",
                super::package_harness::harness_entry_label(entry),
                compact_harness_text(&entry.title, max_content_length),
                compact_harness_text(&entry.path, max_content_length),
                super::package_harness::harness_version_text(entry.version),
                reference_text,
                arguments_text,
                super::package_harness::package_provenance_text(entry, max_content_length),
                compact_harness_text(&entry.content, max_content_length)
            ));
        }
        let overflow = entries.len().saturating_sub(max_entries_per_kind);
        if overflow > 0 {
            lines.push(format!("- +{overflow} more {kind_name} entries"));
        }
        if let Some(adjustment) = adjustment {
            for (group, count) in adjustment.withheld.iter().zip(&withheld_counts) {
                if *count > 0 {
                    lines.push(format!(
                        "- +{count} {} {kind_name} entries ({})",
                        group.label, group.note
                    ));
                }
            }
        }
        lines.push(String::new());
    }
    if total_entries == 0 {
        lines.push("No saved harness entries yet.".to_string());
        lines.push(String::new());
    }
    for section in adjustment.map_or(&[][..], |adjustment| adjustment.sections.as_slice()) {
        let section_lines: Vec<String> = section
            .lines
            .iter()
            .map(|line| super::prompt_hook::sanitize_prompt_line(line, max_content_length))
            .filter(|line| !line.is_empty())
            .collect();
        if section_lines.is_empty() {
            continue;
        }
        lines.push(section.heading.clone());
        lines.extend(section_lines.into_iter().map(|line| format!("- {line}")));
        lines.push(String::new());
    }
    lines.push(format!("recent refinements: {}", state.refinements.len()));
    for event in state
        .refinements
        .iter()
        .rev()
        .take(max_refinements)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        let changes = if event.changes.is_empty() {
            "no applied edits".to_string()
        } else {
            event.changes.join(", ")
        };
        let outcome = if event.outcome.is_empty() {
            String::new()
        } else {
            format!(
                "; outcome: {}",
                compact_harness_text(&event.outcome, max_content_length)
            )
        };
        lines.push(format!(
            "- [{}] {}: {}{}",
            event.id,
            compact_harness_text(&event.trigger, max_content_length),
            changes,
            outcome
        ));
    }
    let refinement_overflow = state.refinements.len().saturating_sub(max_refinements);
    if refinement_overflow > 0 {
        lines.push(format!("- +{refinement_overflow} older refinement events"));
    }
    lines.join("\n").trim().to_string()
}

/// Bump when the fingerprinted material or its canonical serialization
/// changes, so fingerprints minted under different versions never compare
/// equal.
pub const HARNESS_DIGEST_FINGERPRINT_VERSION: u32 = 1;

/// The render flags the digest actually reads: the relevance query terms
/// are excluded — the digest stays frozen per delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessDigestRenderFlags {
    pub include_ipython_examples: bool,
    pub include_shell_examples: bool,
    pub include_refine_examples: bool,
}

fn scope_name(entry: &HarnessEntry) -> &'static str {
    match entry.scope {
        Some(super::HarnessScope::Local) => "local",
        _ => "global",
    }
}

fn refinement_kind_name(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

/// [`harness_digest_fingerprint`] of a render a feature adjusted: the
/// native fingerprint when `adjustment` is `None`, else a hash over it and
/// the adjustment, so a changed adjustment re-delivers the digest.
#[must_use]
pub fn adjusted_harness_digest_fingerprint(
    state: &HarnessState,
    render_flags: HarnessDigestRenderFlags,
    adjustment: Option<&super::prompt_hook::HarnessPromptAdjustment>,
) -> String {
    let native = harness_digest_fingerprint(state, render_flags);
    let Some(adjustment) = adjustment else {
        return native;
    };
    let mut hasher = Sha256::new();
    hasher.update(native.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_string(adjustment)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |hex, byte| hex + &format!("{byte:02x}"))
}

/// Stable fingerprint of the harness material a digest renders (TS
/// `harnessDigestFingerprint`): equal states produce equal fingerprints,
/// so cold boundaries skip re-delivery with a state comparison instead of
/// a rendered-text comparison that query-term relevance keeps invalidating.
#[must_use]
pub fn harness_digest_fingerprint(
    state: &HarnessState,
    render_flags: HarnessDigestRenderFlags,
) -> String {
    // The section key is the renderer's grouping: an entry moved between
    // sections renders differently and must invalidate the digest, even
    // when its `kind` field disagrees with its section.
    let mut entries: Vec<(&'static str, &HarnessEntry)> = state
        .entries
        .iter()
        .flat_map(|(kind, records)| {
            records
                .values()
                .map(move |entry| (refinement_kind_name(*kind), entry))
        })
        .collect();
    entries.sort_by(|(a_kind, a), (b_kind, b)| {
        format!("{}\0{}\0{}", scope_name(a), a_kind, a.id).cmp(&format!(
            "{}\0{}\0{}",
            scope_name(b),
            b_kind,
            b.id
        ))
    });
    let entry_material = |kind: &'static str, entry: &HarnessEntry| {
        let mut material = serde_json::Map::new();
        material.insert("scope".to_string(), serde_json::json!(scope_name(entry)));
        material.insert("kind".to_string(), serde_json::json!(kind));
        material.insert("id".to_string(), serde_json::json!(entry.id));
        material.insert("title".to_string(), serde_json::json!(entry.title));
        material.insert("path".to_string(), serde_json::json!(entry.path));
        material.insert("version".to_string(), serde_json::json!(entry.version));
        material.insert("content".to_string(), serde_json::json!(entry.content));
        // The label and provenance line render from a package entry's
        // provenance, so a provenance-only change re-renders the digest.
        if let Some(provenance) = entry
            .extensions
            .get(super::package_harness::PACKAGE_PROVENANCE_KEY)
        {
            material.insert("provenance".to_string(), provenance.clone());
        }
        // Only skills render the kernel call contract, so another kind can
        // change these fields without changing a single digest byte.
        if entry.kind == RefinementKind::Skill {
            material.insert(
                "reference".to_string(),
                serde_json::Value::Object(entry.reference.clone().into_iter().collect()),
            );
            material.insert(
                "arguments".to_string(),
                serde_json::Value::Object(entry.arguments.clone().into_iter().collect()),
            );
        }
        serde_json::Value::Object(material)
    };
    // Refinements keep their stored order: the formatter renders the newest
    // tail, so an order-only change renders differently.
    let refinement_material = state
        .refinements
        .iter()
        .map(|event| {
            let mut material = serde_json::Map::new();
            material.insert("id".to_string(), serde_json::json!(event.id));
            material.insert("trigger".to_string(), serde_json::json!(event.trigger));
            material.insert("changes".to_string(), serde_json::json!(event.changes));
            material.insert("outcome".to_string(), serde_json::json!(event.outcome));
            serde_json::Value::Object(material)
        })
        .collect::<Vec<_>>();
    // The shell call-contract renders only when IPython examples are
    // absent: fingerprint only the flags the render reads.
    let effective_shell_examples = if render_flags.include_ipython_examples {
        false
    } else {
        render_flags.include_shell_examples
    };
    let mut flags_material = serde_json::Map::new();
    flags_material.insert(
        "includeIpythonExamples".to_string(),
        serde_json::json!(render_flags.include_ipython_examples),
    );
    flags_material.insert(
        "includeShellExamples".to_string(),
        serde_json::json!(effective_shell_examples),
    );
    flags_material.insert(
        "includeRefineExamples".to_string(),
        serde_json::json!(render_flags.include_refine_examples),
    );
    let mut material = serde_json::Map::new();
    material.insert(
        "version".to_string(),
        serde_json::json!(HARNESS_DIGEST_FINGERPRINT_VERSION),
    );
    material.insert(
        "renderFlags".to_string(),
        serde_json::Value::Object(flags_material),
    );
    material.insert(
        "entries".to_string(),
        serde_json::Value::Array(
            entries
                .iter()
                .map(|(kind, entry)| entry_material(kind, entry))
                .collect(),
        ),
    );
    material.insert(
        "refinements".to_string(),
        serde_json::Value::Array(refinement_material),
    );
    let serialized =
        serde_json::to_string(&serde_json::Value::Object(material)).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |hex, byte| hex + &format!("{byte:02x}"))
}

fn kind_for(name: &str) -> RefinementKind {
    match name {
        "prompt" => RefinementKind::Prompt,
        "memory" => RefinementKind::Memory,
        "skill" => RefinementKind::Skill,
        "factory" => RefinementKind::Factory,
        _ => RefinementKind::Subagent,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{empty_harness_state, HarnessScope};
    use super::*;

    #[test]
    fn query_terms_words_and_cjk_bigrams() {
        let terms = harness_query_terms("fix the worktree? then 修复登录问题");
        assert!(terms.contains(&"worktree".to_string()));
        assert!(terms.contains(&"then".to_string()));
        // CJK bigrams overlap so 登录 matches entries mentioning 登录故障.
        assert!(terms.contains(&"修复".to_string()));
        assert!(terms.contains(&"登录".to_string()));
        assert!(!terms.contains(&"fix".to_string())); // short runs drop
        let dupes = harness_query_terms("alpha alpha alpha");
        assert_eq!(dupes.iter().filter(|t| *t == "alpha").count(), 1);
    }

    fn make_entry(id: &str, title: &str, content: &str, path: &str) -> HarnessEntry {
        HarnessEntry {
            id: id.to_string(),
            kind: RefinementKind::Memory,
            title: title.to_string(),
            content: content.to_string(),
            path: path.to_string(),
            scope: Some(HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            extensions: serde_json::Map::new(),
        }
    }

    /// #1118: a disabled entry never reaches the digest; its kind line
    /// counts it as unavailable. Without disabled entries the digest is
    /// unchanged.
    #[test]
    fn disabled_entries_stay_out_of_the_digest() {
        let mut state = crate::refinement::empty_harness_state();
        let active = make_entry("active", "Active", "use tactic A", "general");
        let mut retired = make_entry("retired", "Retired", "stale guidance", "general");
        let memories = state.entries.entry(RefinementKind::Memory).or_default();
        memories.insert(active.id.clone(), active);
        memories.insert(retired.id.clone(), retired.clone());
        let options = HarnessStatePromptOptions::default();
        let all_enabled = format_harness_state_for_prompt(&state, &options);
        assert!(all_enabled.contains("memory: 2\n"), "{all_enabled}");
        retired.set_enabled(false);
        state
            .entries
            .entry(RefinementKind::Memory)
            .or_default()
            .insert(retired.id.clone(), retired);
        let digest = format_harness_state_for_prompt(&state, &options);
        assert!(
            digest.contains("memory: 1 (+1 disabled, not available)\n"),
            "{digest}"
        );
        assert!(digest.contains("Active"), "{digest}");
        assert!(
            !digest.contains("Retired") && !digest.contains("stale guidance"),
            "{digest}"
        );
        // Re-enabled: byte-identical to the never-disabled digest (an
        // explicit `enabled: true` is not a difference).
        let mut restored = state.clone();
        restored
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .get_mut("retired")
            .unwrap()
            .set_enabled(true);
        assert_eq!(
            format_harness_state_for_prompt(&restored, &options),
            all_enabled
        );
    }

    #[test]
    fn scoring_field_coverage() {
        let entry = HarnessEntry {
            id: "web".to_string(),
            kind: RefinementKind::Skill,
            title: "Web Search".to_string(),
            content: "searches the web for results".to_string(),
            path: "/skills/web".to_string(),
            scope: Some(HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            extensions: serde_json::Map::new(),
        };
        let mut terms = HarnessQueryTerms::new();
        terms.insert("web".to_string(), 1.0);
        // web matches title + content + identifier: 3 fields -> 1.0*(1+1.0) = 2.
        let score = score_harness_entry_for_query(&entry, &terms, None);
        assert!((score - 2.0).abs() < 1e-9);
        terms.insert("absent".to_string(), 1.0);
        assert!((score_harness_entry_for_query(&entry, &terms, None) - score).abs() < 1e-9);
    }

    #[test]
    fn idf_discounts_common_terms_over_the_ranked_corpus() {
        let terms = HarnessQueryTerms::from_iter([
            ("session".to_string(), 1.0),
            ("quantum".to_string(), 1.0),
            ("missing".to_string(), 1.0),
        ]);
        let common0 = make_entry("common0", "Session notes", "session text", "general");
        let common1 = make_entry("common1", "Session notes", "session text", "general");
        let rare = make_entry("rare", "Quantum note", "quantum text", "general");
        // "session" matches 2 of 3 entries, "quantum" 1 of 3, "missing" none.
        let corpus = [common0.clone(), common1.clone(), rare.clone()];
        let idf = harness_query_term_idf(&corpus, &terms);
        assert_eq!(idf.len(), 2);
        let documents: f64 = 3.0;
        assert!((idf["session"] - (1.0 + documents / 2.0).ln()).abs() < 1e-9);
        assert!((idf["quantum"] - (1.0 + documents / 1.0).ln()).abs() < 1e-9);
        // The discount scales the weighted overlap: "quantum" covers 2 fields of 1 entry.
        let rare_score = score_harness_entry_for_query(&rare, &terms, Some(&idf));
        assert!((rare_score - (1.0 + documents / 1.0).ln() * 1.5).abs() < 1e-9);
        // A term in every entry still weighs ln(2); degenerate corpora stay
        // inert, and empty terms or corpora score nothing.
        let solo_idf = harness_query_term_idf(std::slice::from_ref(&rare), &terms);
        assert!((solo_idf["quantum"] - 2.0_f64.ln()).abs() < 1e-9);
        assert!(harness_query_term_idf(&[], &terms).is_empty());
        assert!(harness_query_term_idf(&corpus, &HarnessQueryTerms::new()).is_empty());
        // A rare distinctive term outranks a common-term-dense entry in the
        // rendered window, regardless of updated_at recency.
        let mut state = empty_harness_state();
        for entry in [common0, common1, rare] {
            state
                .entries
                .get_mut(&RefinementKind::Memory)
                .unwrap()
                .insert(entry.id.clone(), entry);
        }
        let rendered = format_harness_state_for_prompt(
            &state,
            &HarnessStatePromptOptions {
                max_entries_per_kind: Some(2),
                query_terms: Some(HarnessQueryTerms::from_iter([
                    ("session".to_string(), 1.0),
                    ("quantum".to_string(), 1.0),
                ])),
                ..Default::default()
            },
        );
        assert!(rendered.contains("[global:rare]"));
        assert!(rendered.contains("+1 more memory entries"));
    }

    #[test]
    fn factory_digest_line_renders_with_the_run_watch_stop_hint() {
        // TS shape: the factory line renders only when entries exist and
        // IPython examples are on (the await forms follow the accepted
        // review fix for the coroutine-object pitfall).
        let mut state = empty_harness_state();
        let mut factory = make_entry("sweep", "PR review sweep", "Sweep review.", "review");
        factory.kind = RefinementKind::Factory;
        state
            .entries
            .get_mut(&RefinementKind::Factory)
            .unwrap()
            .insert("sweep".to_string(), factory);
        let rendered = format_harness_state_for_prompt(
            &state,
            &HarnessStatePromptOptions {
                max_entries_per_kind: Some(40),
                query_terms: None,
                include_ipython_examples: Some(true),
                ..Default::default()
            },
        );
        assert!(rendered.contains(
            "factory: 1 (state-machine workflow specs; run one with `await rlm.factory.run('<id>')`; watch with `await rlm.factory.status(run_id)`, stop with `await rlm.factory.stop(run_id)`, resume a paused run with `await rlm.factory.resume(run_id)`)"
        ));
        assert!(rendered.contains("- [global:sweep] PR review sweep (review, v1): Sweep review."));
        // Without IPython examples the hint line stays a plain count.
        let plain = format_harness_state_for_prompt(
            &state,
            &HarnessStatePromptOptions {
                max_entries_per_kind: Some(40),
                query_terms: None,
                include_ipython_examples: Some(false),
                ..Default::default()
            },
        );
        assert!(plain.contains("\nfactory: 1\n"));
        // An empty factory section renders no invoke hint.
        let empty = empty_harness_state();
        let rendered_empty = format_harness_state_for_prompt(
            &empty,
            &HarnessStatePromptOptions {
                max_entries_per_kind: Some(40),
                query_terms: None,
                include_ipython_examples: Some(true),
                ..Default::default()
            },
        );
        assert!(rendered_empty.contains("\nfactory: 0\n"));
        assert!(!rendered_empty.contains("rlm.factory.run"));
        // Fingerprint stability: a factory entry participates like any
        // other kind; its content change re-fingerprints (arguments render
        // only for skills, so an arguments-only change stays inert, exactly
        // like the TS fingerprint material).
        let flags = HarnessDigestRenderFlags {
            include_ipython_examples: true,
            include_shell_examples: false,
            include_refine_examples: false,
        };
        let baseline = harness_digest_fingerprint(&state, flags);
        let mut changed = state.clone();
        changed
            .entries
            .get_mut(&RefinementKind::Factory)
            .unwrap()
            .get_mut("sweep")
            .unwrap()
            .content
            .push_str(" more");
        assert_ne!(harness_digest_fingerprint(&changed, flags), baseline);
    }

    #[test]
    fn fingerprint_is_stable_across_entry_order_and_ignores_query_terms() {
        let mut state = empty_harness_state();
        let alpha = make_entry("alpha", "Alpha note", "Alpha content", "general");
        let zeta = make_entry("zeta", "Zeta note", "Zeta content", "policy");
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("alpha".to_string(), alpha);
        state
            .entries
            .get_mut(&RefinementKind::Prompt)
            .unwrap()
            .insert("zeta".to_string(), zeta);
        let flags = HarnessDigestRenderFlags {
            include_ipython_examples: true,
            include_shell_examples: false,
            include_refine_examples: false,
        };
        let baseline = harness_digest_fingerprint(&state, flags);
        // Kind iteration order is normalized away, as is the invisible
        // metadata/source/timestamps bookkeeping.
        let mut reordered = empty_harness_state();
        let entry = state.entries[&RefinementKind::Memory]["alpha"].clone();
        reordered
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("alpha".to_string(), entry);
        let entry = state.entries[&RefinementKind::Prompt]["zeta"].clone();
        reordered
            .entries
            .get_mut(&RefinementKind::Prompt)
            .unwrap()
            .insert("zeta".to_string(), entry);
        assert_eq!(harness_digest_fingerprint(&reordered, flags), baseline);
        // A content change re-fingerprints (the digest would differ).
        let mut changed = state.clone();
        changed
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .get_mut("alpha")
            .unwrap()
            .content
            .push_str(" more");
        assert_ne!(harness_digest_fingerprint(&changed, flags), baseline);
        // The shell flag cannot change the digest while IPython examples
        // take precedence: it is normalized out of the material.
        let shell_on = HarnessDigestRenderFlags {
            include_ipython_examples: true,
            include_shell_examples: true,
            include_refine_examples: false,
        };
        assert_eq!(harness_digest_fingerprint(&state, shell_on), baseline);
        // Without IPython examples the shell contract renders, so the flag
        // participates; so does the refine flag while IPython is on.
        let shell_only = HarnessDigestRenderFlags {
            include_ipython_examples: false,
            include_shell_examples: true,
            include_refine_examples: false,
        };
        assert_ne!(harness_digest_fingerprint(&state, shell_only), baseline);
    }

    #[test]
    fn fingerprint_covers_skill_contract_and_refinement_order() {
        let mut state = empty_harness_state();
        let mut skill = make_entry("skill_a", "Skill A", "Skill content", "general");
        skill.kind = RefinementKind::Skill;
        skill
            .reference
            .insert("type".to_string(), serde_json::json!("python"));
        state
            .entries
            .get_mut(&RefinementKind::Skill)
            .unwrap()
            .insert("skill_a".to_string(), skill.clone());
        state
            .refinements
            .push(super::super::HarnessRefinementEvent {
                id: "r1".to_string(),
                trigger: "after a repeated failure".to_string(),
                changes: vec!["create skill skill_a".to_string()],
                evidence: String::new(),
                outcome: "routing improved".to_string(),
                created_at: String::new(),
                reason: None,
            });
        state
            .refinements
            .push(super::super::HarnessRefinementEvent {
                id: "r2".to_string(),
                trigger: "a later pass".to_string(),
                changes: vec!["update memory m".to_string()],
                evidence: String::new(),
                outcome: String::new(),
                created_at: String::new(),
                reason: None,
            });
        let flags = HarnessDigestRenderFlags {
            include_ipython_examples: true,
            include_shell_examples: false,
            include_refine_examples: true,
        };
        let baseline = harness_digest_fingerprint(&state, flags);
        // The skill's call contract participates; the same fields on a
        // memory entry never render, so they stay out of the material.
        let mut contract_changed = state.clone();
        contract_changed
            .entries
            .get_mut(&RefinementKind::Skill)
            .unwrap()
            .get_mut("skill_a")
            .unwrap()
            .reference
            .insert("import".to_string(), serde_json::json!("rlm.bash"));
        assert_ne!(
            harness_digest_fingerprint(&contract_changed, flags),
            baseline
        );
        let mut memory_touched = state.clone();
        memory_touched
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m".to_string(),
                make_entry("m", "M", "memory content", "general"),
            );
        // A new memory entry changes the digest, so the fingerprint moves.
        assert_ne!(harness_digest_fingerprint(&memory_touched, flags), baseline);
        // Refinements keep their stored order: a reorder renders a
        // different newest tail and must not reuse the fingerprint.
        let mut reordered = state.clone();
        reordered.refinements.reverse();
        assert_ne!(harness_digest_fingerprint(&reordered, flags), baseline);
        // The material keys `kind` by the printed SECTION: an entry moved
        // between sections renders differently and must invalidate.
        let mut moved = state.clone();
        let moved_skill = moved
            .entries
            .get_mut(&RefinementKind::Skill)
            .unwrap()
            .remove("skill_a")
            .expect("the seeded skill");
        moved
            .entries
            .get_mut(&RefinementKind::Prompt)
            .unwrap()
            .insert("skill_a".to_string(), moved_skill);
        assert_ne!(harness_digest_fingerprint(&moved, flags), baseline);
    }

    #[test]
    fn digest_renders_kinds_and_refinements() {
        let mut state = empty_harness_state();
        let entry = HarnessEntry {
            id: "m1".to_string(),
            kind: RefinementKind::Memory,
            title: "Fact".to_string(),
            content: "the  build is green".to_string(),
            path: "/m/m1".to_string(),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 2,
            extensions: serde_json::Map::new(),
        };
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("m1".to_string(), entry);
        state
            .refinements
            .push(super::super::HarnessRefinementEvent {
                id: "r1".to_string(),
                trigger: "after a repeated failure".to_string(),
                changes: vec!["create memory m1".to_string()],
                evidence: String::new(),
                outcome: "routing improved".to_string(),
                created_at: String::new(),
                reason: None,
            });
        let text = format_harness_state_for_prompt(&state, &HarnessStatePromptOptions::default());
        assert!(text.starts_with("# Continual Harness State"));
        assert!(text.contains("memory: 1"));
        assert!(text.contains("- [local:m1] Fact (/m/m1, v2): the build is green"));
        assert!(text.contains("recent refinements: 1"));
        assert!(text.contains("routing improved"));
        assert!(text.contains("When to call `await refine.run()`"));
        let empty_text = format_harness_state_for_prompt(
            &empty_harness_state(),
            &HarnessStatePromptOptions::default(),
        );
        assert!(empty_text.contains("No saved harness entries yet."));
    }
}
