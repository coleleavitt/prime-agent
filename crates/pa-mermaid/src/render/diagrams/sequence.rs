//! `sequenceDiagram`: participants, messages, notes, block dividers, and activations.
//! Ported from lovely-mermaid 0.3.3 `diagrams/sequence.ts` (Apache-2.0; see
//! `LICENSE-lovely-mermaid`).
//!
//! Lenient: an unreadable statement is dropped and recorded. Sequence diagrams have their
//! own model — participants in declaration order plus a flat list of items — and their
//! own geometry (`layout_seq`).

use std::collections::HashMap;

use super::super::graph::{MAX_EDGES, MAX_NODES};
use super::super::js_text;
use super::super::labels::{ascii_lower, clean_label, decode_html_entities};
use super::super::layout_seq::layout_sequence;
use super::super::statements::{first_word, header_kind, non_empty, statements_of};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["sequencediagram"];

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let seq = parse_sequence(src)?;
    let canvas = layout_sequence(&seq)?;
    Some(Drawn {
        canvas,
        warnings: seq.warnings,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::render) enum SeqHead {
    Arrow,
    Cross,
}

/// Message operators, longest first so `-->>` wins over `-->`: `(op, dashed, head)`.
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
const SEQ_IGNORED: [&str; 8] = [
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
pub(in crate::render) enum NoteAnchor {
    Over { from: usize, to: usize },
    Left { at: usize },
    Right { at: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::render) enum SeqItem {
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

/// A hot span of one lifeline: item indices, `to` unset while still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::render) struct Activation {
    pub(in crate::render) at: usize,
    pub(in crate::render) from: usize,
    pub(in crate::render) to: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub(in crate::render) struct Sequence {
    pub(in crate::render) labels: Vec<String>,
    index: HashMap<String, usize>,
    pub(in crate::render) items: Vec<SeqItem>,
    pub(in crate::render) activations: Vec<Activation>,
    /// The cap that was hit, if any: the parser stops and renders the prefix.
    truncated: Option<String>,
    /// Statements the grammar could not read and dropped.
    warnings: Vec<String>,
}

impl Sequence {
    fn truncate(&mut self, cap: String) {
        if self.truncated.is_none() {
            self.truncated = Some(cap);
        }
    }

    fn participant(&mut self, id: &str, label: Option<String>) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            if let Some(label) = label {
                self.labels[existing] = label;
            }
            return Some(existing);
        }
        if self.labels.len() >= MAX_NODES {
            self.truncate(format!("participant cap ({MAX_NODES}) reached"));
            return None;
        }
        self.index.insert(id.to_owned(), self.labels.len());
        self.labels.push(label.unwrap_or_else(|| id.to_owned()));
        Some(self.labels.len() - 1)
    }

    /// Append an item, or flag `truncated` when the item cap is reached.
    fn push_item(&mut self, item: SeqItem) {
        if self.items.len() >= MAX_EDGES {
            self.truncate(format!("item cap ({MAX_EDGES}) reached"));
        } else {
            self.items.push(item);
        }
    }

    /// Record an unreadable statement; skipped once truncated.
    fn drop_statement(&mut self, st: &str) {
        if self.truncated.is_none() {
            self.warnings
                .push(format!("dropped, unreadable statement: \"{st}\""));
        }
    }

    /// Open an activation of `at` at item `from`.
    fn activate(&mut self, at: usize, from: usize) {
        self.activations.push(Activation { at, from, to: None });
    }

    /// Close the innermost open activation of `at` at item `to`.
    fn deactivate(&mut self, at: usize, to: usize) {
        if let Some(a) = self
            .activations
            .iter_mut()
            .rev()
            .find(|a| a.at == at && a.to.is_none())
        {
            a.to = Some(to);
        }
    }
}

fn parse_sequence(src: &str) -> Option<Sequence> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if !HEADERS.contains(&kind.as_str()) {
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
                seq.drop_statement(st);
            } else {
                let (id, label) = match rest.split_once(" as ") {
                    Some((id, label)) => (js_text::trim(id), Some(clean_label(label))),
                    None => (rest, None),
                };
                seq.participant(id, label);
            }
        } else if lower == "autonumber" {
            autonumber = true;
        } else if lower == "activate" || lower == "deactivate" {
            // Applies to the preceding message's row.
            let who = seq.index.get(js_text::trim(&st[first.len()..])).copied();
            if let (Some(who), Some(last)) = (who, seq.items.len().checked_sub(1)) {
                if lower == "activate" {
                    seq.activate(who, last);
                } else {
                    seq.deactivate(who, last);
                }
            }
        } else if SEQ_IGNORED.contains(&lower) {
            // No layout meaning.
        } else if lower == "note" {
            match parse_note_anchor(js_text::trim(&st[first.len()..]), &mut seq) {
                Some((anchor, text)) => seq.push_item(SeqItem::Note { anchor, text }),
                None => seq.drop_statement(st),
            }
        } else if SEQ_BLOCKS.contains(&lower) {
            // A continuation only divides a block that opened one.
            let continues = SEQ_CONTINUATIONS.contains(&lower);
            if !continues {
                blocks.push(true);
            }
            if !continues || blocks.last() == Some(&true) {
                seq.push_item(SeqItem::Divider {
                    text: decode_html_entities(st),
                });
            }
        } else if lower == "rect" || lower == "box" {
            blocks.push(false);
        } else if lower == "end" {
            if blocks.pop() == Some(true) {
                seq.push_item(SeqItem::Divider {
                    text: "end".to_owned(),
                });
            }
        } else {
            match parse_seq_message(st, &mut seq) {
                None => seq.drop_statement(st),
                Some(msg) => {
                    let mut text = msg.text;
                    if autonumber {
                        msg_count += 1;
                        text = Some(match text {
                            None => format!("{msg_count}."),
                            Some(text) => format!("{msg_count}. {text}"),
                        });
                    }
                    let item = seq.items.len();
                    seq.push_item(SeqItem::Message {
                        from: msg.from,
                        to: msg.to,
                        text,
                        dashed: msg.dashed,
                        head: msg.head,
                    });
                    // `+` activates the receiver on this row; `-` deactivates the sender —
                    // only once the item was accepted.
                    if seq.items.len() == item + 1 {
                        if msg.marks.contains('+') {
                            seq.activate(msg.to, item);
                        }
                        if msg.marks.contains('-') {
                            seq.deactivate(msg.from, item);
                        }
                    }
                }
            }
        }
        if let Some(cap) = &seq.truncated {
            let warning = format!("diagram truncated: {cap}");
            seq.warnings.push(warning);
            break;
        }
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

    let (ids, text) = rest[prefix..].split_once(':')?;
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
    /// The `+`/`-` activation marks after the operator.
    marks: String,
}

fn parse_seq_message(st: &str, seq: &mut Sequence) -> Option<Message> {
    let chars: Vec<char> = st.chars().collect();
    let mut found = None;
    'outer: for pos in 0..chars.len() {
        let tail: String = chars[pos..(pos + MAX_SEQ_OP).min(chars.len())]
            .iter()
            .collect();
        for &(op, dashed, head) in &SEQ_OPS {
            if tail.starts_with(op) {
                // `-x` / `-)` embedded in a hyphenated token (`pre-x->>B`) is not an
                // operator — the real one follows.
                let after = chars.get(pos + op.len());
                if (op.ends_with('x') || op.ends_with(')')) && matches!(after, Some('-' | '>')) {
                    continue;
                }
                found = Some((pos, op, dashed, head));
                break 'outer;
            }
        }
    }
    let (pos, op, dashed, head) = found?;

    let from_raw: String = chars[..pos].iter().collect();
    let from_id = js_text::trim(&from_raw);
    if from_id.is_empty() {
        return None;
    }
    let after_raw: String = chars[pos + op.len()..].iter().collect();
    let after_op = js_text::trim_start(&after_raw);
    let rest = after_op.trim_start_matches(['+', '-']);
    let marks = after_op[..after_op.len() - rest.len()].to_owned();

    let (to_id, text) = match rest.split_once(':') {
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
        marks,
    })
}
