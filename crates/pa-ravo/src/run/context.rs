//! The bounded context view a RAVO run's children see (TS
//! `ravo/context-view.ts`, for the text atoms a run builds): the current
//! task first, then the champion and the constraints, each cut to the
//! byte, token and per-item bounds, with a digest over the view.

use serde_json::{json, Map, Value};

use crate::js::{json_string, locale_compare, sha256_hex};

/// What an atom is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAtomKind {
    CurrentTask,
    Champion,
    Constraint,
}

impl ContextAtomKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CurrentTask => "current_task",
            Self::Champion => "champion",
            Self::Constraint => "constraint",
        }
    }
}

/// One text atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAtom {
    pub id: String,
    pub kind: ContextAtomKind,
    pub text: String,
}

/// Everything a run could show its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextArchive {
    pub current_task: ContextAtom,
    pub champion: Option<ContextAtom>,
    pub constraints: Vec<ContextAtom>,
}

/// The view's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextViewLimits {
    pub max_tokens: u64,
    pub max_bytes: u64,
    pub max_items: u64,
    pub lineage_depth: u64,
    pub max_artifact_bytes_per_item: u64,
}

/// One shown atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextViewItem {
    pub id: String,
    pub kind: ContextAtomKind,
    pub content: Option<String>,
    pub reasons: Vec<&'static str>,
    pub bytes: u64,
    pub sha256: String,
}

/// The bounded view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedContextView {
    pub items: Vec<ContextViewItem>,
    pub sha256: String,
}

/// `text` cut on code-point boundaries to at most `max_bytes` UTF-8 bytes.
fn truncate_utf8(text: &str, max_bytes: u64) -> (&str, bool) {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if text.len() <= max {
        return (text, false);
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > max {
            break;
        }
        end = index + ch.len_utf8();
    }
    (&text[..end], true)
}

/// The TS `canonical`: keys sorted by `localeCompare`, absent fields
/// dropped, values as `JSON.stringify` writes them.
fn canonical(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|left, right| locale_compare(left, right));
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|key| format!("{}:{}", json_string(key), canonical(&map[*key])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => other.to_string(),
    }
}

fn byte_len(text: &str) -> u64 {
    text.len() as u64
}

/// Build the view: atoms in order (task, champion, constraints), each cut
/// to what the bounds leave; tokens are counted as UTF-8 bytes.
#[must_use]
pub fn build_bounded_context_view(
    archive: &ContextArchive,
    limits: &ContextViewLimits,
) -> BoundedContextView {
    let atoms = std::iter::once(&archive.current_task)
        .chain(archive.champion.as_ref())
        .chain(&archive.constraints);
    let mut items: Vec<ContextViewItem> = Vec::new();
    let mut omissions: Vec<(&'static str, u64)> = Vec::new();
    let mut omit =
        |reason: &'static str| match omissions.iter_mut().find(|(known, _)| *known == reason) {
            Some((_, count)) => *count += 1,
            None => omissions.push((reason, 1)),
        };
    let (mut used_bytes, mut used_tokens) = (0u64, 0u64);
    let mut values: Vec<Value> = Vec::new();
    for atom in atoms {
        if atom.id.is_empty() || atom.text.is_empty() {
            omit("invalid_atom");
            continue;
        }
        if items.len() as u64 >= limits.max_items {
            omit("max_items");
            continue;
        }
        let remaining_bytes = limits.max_bytes.saturating_sub(used_bytes);
        let remaining_tokens = limits.max_tokens.saturating_sub(used_tokens);
        if remaining_bytes == 0 || remaining_tokens == 0 {
            omit(if remaining_bytes == 0 {
                "max_bytes"
            } else {
                "max_tokens"
            });
            continue;
        }
        let mut reasons: Vec<&'static str> = Vec::new();
        let (cut, truncated) = truncate_utf8(
            &atom.text,
            remaining_bytes.min(limits.max_artifact_bytes_per_item),
        );
        if truncated {
            reasons.push(if remaining_bytes <= limits.max_artifact_bytes_per_item {
                "max_bytes"
            } else {
                "artifact_too_large"
            });
        }
        let mut content = cut;
        // One token per byte: the token cut is a byte cut.
        if byte_len(content) > remaining_tokens {
            content = truncate_utf8(content, remaining_tokens).0;
            reasons.push("max_tokens");
        }
        let bytes = byte_len(content);
        let id = truncate_utf8(&atom.id, limits.max_artifact_bytes_per_item)
            .0
            .to_string();
        let mut base = Map::new();
        base.insert("id".into(), json!(id));
        base.insert("kind".into(), json!(atom.kind.as_str()));
        if !content.is_empty() {
            base.insert("content".into(), json!(content));
        }
        base.insert("redactions".into(), json!([]));
        base.insert("truncated".into(), json!(!reasons.is_empty()));
        base.insert("reasons".into(), json!(reasons));
        base.insert("bytes".into(), json!(bytes));
        base.insert("tokens".into(), json!(bytes));
        let sha256 = sha256_hex(&canonical(&Value::Object(base.clone())));
        base.insert("sha256".into(), json!(sha256));
        values.push(Value::Object(base));
        items.push(ContextViewItem {
            id,
            kind: atom.kind,
            content: (!content.is_empty()).then(|| content.to_string()),
            reasons,
            bytes,
            sha256,
        });
        used_bytes += bytes;
        used_tokens += bytes;
    }
    omissions.sort_by(|left, right| locale_compare(left.0, right.0));
    let body = json!({
        "items": values,
        "omissions": omissions
            .iter()
            .map(|(reason, count)| json!({"reason": reason, "count": count}))
            .collect::<Vec<_>>(),
        "usage": {"bytes": used_bytes, "tokens": used_tokens, "items": items.len()},
        "limits": {
            "maxTokens": limits.max_tokens,
            "maxBytes": limits.max_bytes,
            "maxItems": limits.max_items,
            "lineageDepth": limits.lineage_depth,
            "maxArtifactBytesPerItem": limits.max_artifact_bytes_per_item,
        },
    });
    BoundedContextView {
        items,
        sha256: sha256_hex(&canonical(&body)),
    }
}

/// The view as the children's prompts carry it.
#[must_use]
pub fn render_context(view: &BoundedContextView) -> String {
    view.items
        .iter()
        .filter_map(|item| {
            let content = item.content.as_ref()?;
            let kind = item.kind.as_str();
            Some(format!("<{kind} id=\"{}\">\n{content}\n</{kind}>", item.id))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
