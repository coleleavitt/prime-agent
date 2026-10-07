//! Entry id minting and term search over the kernel's harness store.

use super::pyfmt;
use crate::refinement::HarnessEntry;

/// The id a create without one mints from the title: lowercase letters and
/// digits of any script, every other run collapsed to `_`, at most 80
/// characters, `fallback` (the kind) when nothing is left.
pub(crate) fn slug(raw: &str, fallback: &str) -> String {
    let normalized: String = raw
        .chars()
        .flat_map(|ch| {
            if pyfmt::is_alnum(ch) {
                ch.to_lowercase().collect::<Vec<char>>()
            } else {
                vec!['_']
            }
        })
        .collect();
    let joined = normalized
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let chosen = if joined.is_empty() { fallback } else { &joined };
    chosen.chars().take(80).collect()
}

/// CJK characters: spacing-free scripts whose runs are cut into bigrams.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3040}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{20000}'..='\u{2a6df}'
            | '\u{2a700}'..='\u{2b73f}'
            | '\u{2b740}'..='\u{2b81f}'
            | '\u{2b820}'..='\u{2ceaf}'
            | '\u{2ceb0}'..='\u{2ebef}'
            | '\u{2ebf0}'..='\u{2ee5f}'
            | '\u{2f800}'..='\u{2fa1f}'
            | '\u{30000}'..='\u{3134f}'
            | '\u{31350}'..='\u{323af}'
            | '\u{323b0}'..='\u{3347f}'
    )
}

/// Split lowercase text into word runs: letters, digits, and combining
/// marks of any script share a run; punctuation and symbols end it; a run
/// also breaks where CJK meets other script (accented Latin stays whole,
/// `修复login` splits).
fn query_runs(text: &str) -> Vec<Vec<char>> {
    let mut runs = Vec::new();
    let mut run: Vec<char> = Vec::new();
    let mut run_is_cjk = false;
    for ch in text.chars() {
        if pyfmt::is_mark(ch) || pyfmt::is_alnum(ch) {
            let ch_is_cjk = is_cjk(ch);
            if !run.is_empty() && ch_is_cjk != run_is_cjk {
                runs.push(std::mem::take(&mut run));
            }
            run_is_cjk = ch_is_cjk;
            run.push(ch);
        } else if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

/// Tokenize a query into distinct lowercase substring terms: CJK runs
/// become overlapping bigrams, ASCII terms need three characters and other
/// scripts two.
pub(crate) fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for run in query_runs(&query.to_lowercase()) {
        let candidates: Vec<String> = if run.iter().copied().any(is_cjk) {
            if run.len() < 2 {
                vec![run.iter().collect()]
            } else {
                run.windows(2).map(|pair| pair.iter().collect()).collect()
            }
        } else if run.iter().all(char::is_ascii) {
            if run.len() >= 3 {
                vec![run.iter().collect()]
            } else {
                Vec::new()
            }
        } else if run.len() >= 2 {
            vec![run.iter().collect()]
        } else {
            Vec::new()
        };
        for term in candidates {
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

/// Rank `entries` by tf-idf weighted term overlap with `terms`: a term
/// matched in more of an entry's fields (title, content, path and id)
/// counts more, a term found in fewer entries weighs more, and equal scores
/// keep the newest first. Zero-score entries are dropped.
pub(crate) fn rank<'a>(
    entries: &[&'a HarnessEntry],
    terms: &[String],
    limit: usize,
) -> Vec<&'a HarnessEntry> {
    let fields: Vec<[String; 3]> = entries
        .iter()
        .map(|entry| {
            [
                entry.title.to_lowercase(),
                entry.content.to_lowercase(),
                format!("{} {}", entry.path, entry.id).to_lowercase(),
            ]
        })
        .collect();
    let corpus = entries.len() as f64;
    let term_idf: Vec<(&str, f64)> = terms
        .iter()
        .filter_map(|term| {
            let count = fields
                .iter()
                .filter(|fields| fields.iter().any(|field| field.contains(term.as_str())))
                .count();
            (count > 0).then(|| (term.as_str(), (1.0 + corpus / count as f64).ln()))
        })
        .collect();
    let mut scored: Vec<(f64, &'a HarnessEntry)> = entries
        .iter()
        .zip(&fields)
        .map(|(entry, fields)| {
            let score = term_idf.iter().fold(0.0, |total, (term, idf)| {
                let matched = fields.iter().filter(|field| field.contains(term)).count();
                if matched == 0 {
                    total
                } else {
                    total + idf * (1.0 + (matched as f64 - 1.0) * 0.5)
                }
            });
            (score, *entry)
        })
        .collect();
    // Stable descending sort: equal (score, recency) keys keep list order.
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    scored
        .into_iter()
        .filter(|(score, _)| *score > 0.0)
        .take(limit)
        .map(|(_, entry)| entry)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_the_kernel() {
        assert_eq!(
            slug("Prefer focused patches", "memory"),
            "prefer_focused_patches"
        );
        assert_eq!(slug("  --!!  ", "memory"), "memory");
        assert_eq!(slug("ÉTÉ Notes²", "prompt"), "été_notes²");
        // A Devanagari vowel sign is not alphanumeric in Python.
        assert_eq!(slug("किताब", "memory"), "क_त_ब");
        assert_eq!(slug(&"x".repeat(100), "memory").chars().count(), 80);
    }

    #[test]
    fn query_terms_match_the_kernel() {
        let cases: [(&str, &[&str]); 7] = [
            ("worktree branches", &["worktree", "branches"]),
            ("worktree? rlm a ab", &["worktree", "rlm"]),
            ("修复登录", &["修复", "复登", "登录"]),
            ("修复login", &["修复", "login"]),
            ("𠀀", &["𠀀"]),
            ("и мир", &["мир"]),
            ("naïve naïve", &["naïve"]),
        ];
        for (query, expected) in cases {
            assert_eq!(query_terms(query), expected, "{query}");
        }
        assert_eq!(query_terms("किताब"), vec!["किताब".to_string()]);
        assert!(query_terms("??? / . ,").is_empty());
    }
}
