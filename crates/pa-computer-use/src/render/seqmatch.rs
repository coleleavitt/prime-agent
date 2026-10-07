//! `CPython`'s `difflib.SequenceMatcher(None, a, b, autojunk=False)` opcodes.
//!
//! The diff the model reads is defined by this algorithm's choices (its
//! longest-match tie-breaking in particular), so this is the exact
//! `CPython` 3.11 procedure, not a different diff that happens to be valid.

use std::collections::HashMap;

/// One `get_opcodes()` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

/// One opcode: `a[i1..i2]` against `b[j1..j2]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Opcode {
    pub tag: Tag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

/// `SequenceMatcher(None, a, b, autojunk=False).get_opcodes()`.
pub(crate) fn opcodes<T: Eq + std::hash::Hash>(a: &[T], b: &[T]) -> Vec<Opcode> {
    let mut b2j: HashMap<&T, Vec<usize>> = HashMap::new();
    for (index, element) in b.iter().enumerate() {
        b2j.entry(element).or_default().push(index);
    }
    let mut answer = Vec::new();
    let (mut i, mut j) = (0, 0);
    for (ai, bj, size) in matching_blocks(a, b, &b2j) {
        let tag = if i < ai && j < bj {
            Some(Tag::Replace)
        } else if i < ai {
            Some(Tag::Delete)
        } else if j < bj {
            Some(Tag::Insert)
        } else {
            None
        };
        if let Some(tag) = tag {
            answer.push(Opcode {
                tag,
                i1: i,
                i2: ai,
                j1: j,
                j2: bj,
            });
        }
        i = ai + size;
        j = bj + size;
        if size > 0 {
            answer.push(Opcode {
                tag: Tag::Equal,
                i1: ai,
                i2: i,
                j1: bj,
                j2: j,
            });
        }
    }
    answer
}

#[allow(clippy::many_single_char_names)] // CPython's own names, kept for reading against it
fn matching_blocks<T: Eq + std::hash::Hash>(
    a: &[T],
    b: &[T],
    b2j: &HashMap<&T, Vec<usize>>,
) -> Vec<(usize, usize, usize)> {
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut blocks = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(a, b, b2j, alo, ahi, blo, bhi);
        if k > 0 {
            blocks.push((i, j, k));
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    blocks.sort_unstable();
    let mut collapsed = Vec::new();
    let (mut i1, mut j1, mut k1) = (0, 0, 0);
    for (i2, j2, k2) in blocks {
        if i1 + k1 == i2 && j1 + k1 == j2 {
            k1 += k2;
        } else {
            if k1 > 0 {
                collapsed.push((i1, j1, k1));
            }
            (i1, j1, k1) = (i2, j2, k2);
        }
    }
    if k1 > 0 {
        collapsed.push((i1, j1, k1));
    }
    collapsed.push((a.len(), b.len(), 0));
    collapsed
}

/// `find_longest_match(alo, ahi, blo, bhi)` without junk: the earliest
/// longest block in `a`, then the earliest in `b`.
fn longest_match<T: Eq + std::hash::Hash>(
    a: &[T],
    b: &[T],
    b2j: &HashMap<&T, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut best_i, mut best_j, mut best_size) = (alo, blo, 0);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, element) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut next: HashMap<usize, usize> = HashMap::new();
        if let Some(indices) = b2j.get(element) {
            for &j in indices {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|previous| j2len.get(&previous))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                next.insert(j, k);
                if k > best_size {
                    (best_i, best_j, best_size) = (i + 1 - k, j + 1 - k, k);
                }
            }
        }
        j2len = next;
    }
    // CPython then extends the block over equal neighbours; without junk
    // or popular elements every equal neighbour is already in b2j, so the
    // block found is maximal, but the extension is kept for exactness.
    while best_i > alo && best_j > blo && a[best_i - 1] == b[best_j - 1] {
        best_i -= 1;
        best_j -= 1;
        best_size += 1;
    }
    while best_i + best_size < ahi
        && best_j + best_size < bhi
        && a[best_i + best_size] == b[best_j + best_size]
    {
        best_size += 1;
    }
    (best_i, best_j, best_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(a: &str, b: &str) -> Vec<(char, usize, usize, usize, usize)> {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        opcodes(&a, &b)
            .into_iter()
            .map(|op| {
                let tag = match op.tag {
                    Tag::Equal => 'e',
                    Tag::Replace => 'r',
                    Tag::Delete => 'd',
                    Tag::Insert => 'i',
                };
                (tag, op.i1, op.i2, op.j1, op.j2)
            })
            .collect()
    }

    #[test]
    fn opcodes_match_cpython_difflib() {
        // CPython 3.11: SequenceMatcher(None, a, b, autojunk=False).get_opcodes()
        assert_eq!(
            tags("qabxcd", "abycdf"),
            [
                ('d', 0, 1, 0, 0),
                ('e', 1, 3, 0, 2),
                ('r', 3, 4, 2, 3),
                ('e', 4, 6, 3, 5),
                ('i', 6, 6, 5, 6)
            ]
        );
        assert_eq!(
            tags("abcabba", "cbabac"),
            [
                ('i', 0, 0, 0, 2),
                ('e', 0, 2, 2, 4),
                ('i', 2, 2, 4, 5),
                ('e', 2, 3, 5, 6),
                ('d', 3, 7, 6, 6)
            ]
        );
        assert_eq!(tags("", "ab"), [('i', 0, 0, 0, 2)]);
        assert_eq!(tags("ab", ""), [('d', 0, 2, 0, 0)]);
        assert_eq!(tags("same", "same"), [('e', 0, 4, 0, 4)]);
        assert_eq!(tags("", ""), []);
    }
}
