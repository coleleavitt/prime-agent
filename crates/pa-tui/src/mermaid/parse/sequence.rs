//! The sequence-diagram grammar and model. Ported from grok-mermaid 0.2.3 `parse.ts`
//! (Apache-2.0; see `LICENSE-grok-mermaid`).

use std::collections::HashMap;

use super::super::graph::{MAX_EDGES, MAX_NODES};
use super::super::js_text;
use super::super::labels::{ascii_lower, clean_label, decode_html_entities};
use super::{collect, first_word, header_kind, non_empty, split_once, statements_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::mermaid) enum SeqHead {
    Arrow,
    Cross,
}

/// Message operators, longest-first so `-->>` wins over `-->`: `(op, dashed, head)`.
const SEQ_OPS: [(&str, bool, SeqHead); 8] = [
    ("-->>", true, SeqHead::Arrow),
    ("->>", false, SeqHead::Arrow),
    ("--x", true, SeqHead::Cross),
    ("-x", false, SeqHead::Cross),
    ("--)", true, SeqHead::Arrow),
    ("-)", false, SeqHead::Arrow),
    ("-->", true, SeqHead::Arrow),
    ("->", false, SeqHead::Arrow),
];

const MAX_SEQ_OP: usize = 4;

/// Statements a sequence diagram carries no layout meaning for.
const SEQ_IGNORED: [&str; 10] = [
    "activate",
    "deactivate",
    "create",
    "destroy",
    "title",
    "acctitle",
    "accdescr",
    "links",
    "link",
    "properties",
];

/// Block keywords that draw a divider; the last three only continue an open block.
const SEQ_BLOCKS: [&str; 9] = [
    "loop", "alt", "opt", "par", "critical", "break", "else", "and", "option",
];
const SEQ_CONTINUATIONS: [&str; 3] = ["else", "and", "option"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::mermaid) enum NoteAnchor {
    Over { from: usize, to: usize },
    Left { at: usize },
    Right { at: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mermaid) enum SeqItem {
    Message {
        from: usize,
        to: usize,
        text: Option<String>,
        dashed: bool,
        head: SeqHead,
    },
    Note {
        anchor: NoteAnchor,
        text: String,
    },
    Divider {
        text: String,
    },
}

#[derive(Debug, Clone, Default)]
pub(in crate::mermaid) struct Sequence {
    pub(in crate::mermaid) labels: Vec<String>,
    index: HashMap<String, usize>,
    pub(in crate::mermaid) items: Vec<SeqItem>,
}

impl Sequence {
    fn participant(&mut self, id: &str, label: Option<String>) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            if let Some(label) = label {
                self.labels[existing] = label;
            }
            return Some(existing);
        }
        if self.labels.len() >= MAX_NODES {
            return None;
        }
        self.index.insert(id.to_owned(), self.labels.len());
        self.labels.push(label.unwrap_or_else(|| id.to_owned()));
        Some(self.labels.len() - 1)
    }

    /// Append an item, or `None` when the item cap is reached.
    fn push(&mut self, item: SeqItem) -> Option<()> {
        if self.items.len() >= MAX_EDGES {
            return None;
        }
        self.items.push(item);
        Some(())
    }
}

pub(in crate::mermaid) fn parse_sequence(src: &str) -> Option<Sequence> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "sequencediagram" {
        return None;
    }

    let mut seq = Sequence::default();
    let mut autonumber = false;
    let mut msg_count = 0usize;
    // One entry per open block; `true` when it draws a divider on `end`.
    let mut blocks: Vec<bool> = Vec::new();

    for st in &statements[1..] {
        let first = first_word(st);
        let lower = ascii_lower(first);
        let lower = lower.as_str();

        if lower == "participant" || lower == "actor" {
            let rest = js_text::trim(&st[first.len()..]);
            if rest.is_empty() {
                return None;
            }
            let (id, label) = match split_once(rest, " as ") {
                Some((id, label)) => (js_text::trim(id), Some(clean_label(label))),
                None => (rest, None),
            };
            seq.participant(id, label)?;
            continue;
        }
        if lower == "autonumber" {
            autonumber = true;
            continue;
        }
        if SEQ_IGNORED.contains(&lower) {
            continue;
        }
        if lower == "note" {
            let (anchor, text) = parse_note_anchor(js_text::trim(&st[first.len()..]), &mut seq)?;
            seq.push(SeqItem::Note { anchor, text })?;
            continue;
        }
        if SEQ_BLOCKS.contains(&lower) {
            if SEQ_CONTINUATIONS.contains(&lower) {
                // A continuation only divides a block that opened one.
                if blocks.last() != Some(&true) {
                    continue;
                }
            } else {
                blocks.push(true);
            }
            seq.push(SeqItem::Divider {
                text: decode_html_entities(st),
            })?;
            continue;
        }
        if lower == "rect" || lower == "box" {
            blocks.push(false);
            continue;
        }
        if lower == "end" {
            if blocks.pop() == Some(true) {
                seq.push(SeqItem::Divider {
                    text: "end".to_owned(),
                })?;
            }
            continue;
        }

        let msg = parse_seq_message(st, &mut seq)?;
        let mut text = msg.text;
        if autonumber {
            msg_count += 1;
            text = Some(match text {
                None => format!("{msg_count}."),
                Some(text) => format!("{msg_count}. {text}"),
            });
        }
        seq.push(SeqItem::Message {
            from: msg.from,
            to: msg.to,
            text,
            dashed: msg.dashed,
            head: msg.head,
        })?;
    }

    (!seq.labels.is_empty()).then_some(seq)
}

fn parse_note_anchor(rest: &str, seq: &mut Sequence) -> Option<(NoteAnchor, String)> {
    #[derive(Clone, Copy)]
    enum Kind {
        Over,
        Left,
        Right,
    }
    let lower = ascii_lower(rest);
    let (kind, prefix) = if lower.starts_with("over ") {
        (Kind::Over, "over ".len())
    } else if lower.starts_with("left of ") {
        (Kind::Left, "left of ".len())
    } else if lower.starts_with("right of ") {
        (Kind::Right, "right of ".len())
    } else {
        return None;
    };

    let (ids, text) = split_once(&rest[prefix..], ":")?;
    let text = decode_html_entities(js_text::trim(text));
    let parts: Vec<&str> = ids
        .split(',')
        .map(js_text::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let a = seq.participant(parts.first()?, None)?;

    let anchor = match kind {
        Kind::Left => NoteAnchor::Left { at: a },
        Kind::Right => NoteAnchor::Right { at: a },
        Kind::Over => {
            let b = match parts.get(1) {
                Some(second) => seq.participant(second, None)?,
                None => a,
            };
            NoteAnchor::Over {
                from: a.min(b),
                to: a.max(b),
            }
        }
    };
    Some((anchor, text))
}

struct Message {
    from: usize,
    to: usize,
    text: Option<String>,
    dashed: bool,
    head: SeqHead,
}

fn parse_seq_message(st: &str, seq: &mut Sequence) -> Option<Message> {
    let chars: Vec<char> = st.chars().collect();
    let mut found = None;
    'outer: for pos in 0..chars.len() {
        let tail = collect(&chars[pos..(pos + MAX_SEQ_OP).min(chars.len())]);
        for &(op, dashed, head) in &SEQ_OPS {
            if tail.starts_with(op) {
                found = Some((pos, op, dashed, head));
                break 'outer;
            }
        }
    }
    let (pos, op, dashed, head) = found?;

    let from_raw = collect(&chars[..pos]);
    let from_id = js_text::trim(&from_raw);
    if from_id.is_empty() {
        return None;
    }
    // `+`/`-` activate and deactivate the target; they carry no layout meaning.
    let rest_raw = collect(&chars[pos + op.len()..]);
    let rest = js_text::trim_start(&rest_raw).trim_start_matches(['+', '-']);

    let (to_id, text) = match split_once(rest, ":") {
        Some((to_id, text)) => (to_id, non_empty(decode_html_entities(js_text::trim(text)))),
        None => (rest, None),
    };
    let to_id = js_text::trim(to_id);
    if to_id.is_empty() {
        return None;
    }

    let from = seq.participant(from_id, None)?;
    let to = seq.participant(to_id, None)?;
    Some(Message {
        from,
        to,
        text,
        dashed,
        head,
    })
}
