//! The shared statement layer: source text to statements, plus the small string-reading
//! helpers every grammar leans on. Ported from lovely-mermaid 0.3.3 `statements.ts`
//! (Apache-2.0; see `LICENSE-lovely-mermaid`).

use super::js_text;
use super::labels::{ascii_lower, src_lines};

/// Split one source line into statements on `;`, stopping at a `%%` comment. Quoted spans
/// are opaque, so a label may contain `;` and `%%`.
fn split_statements(line: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    let mut cur = String::new();
    let mut in_quotes = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let trimmed = js_text::trim(cur);
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
        cur.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            }
            cur.push(c);
        } else if c == '"' {
            in_quotes = true;
            cur.push(c);
        } else if c == '%' && chars.get(i + 1) == Some(&'%') {
            break;
        } else if c == ';' {
            flush(&mut cur, out);
        } else {
            cur.push(c);
        }
        i += 1;
    }
    flush(&mut cur, out);
}

/// Index just past a leading YAML frontmatter block (`---` … `---`), or 0 when there is
/// none. While the block is still unterminated everything is frontmatter, so a streamed
/// diagram stays blank until it closes (the index may then exceed the line count).
pub(super) fn frontmatter_end(lines: &[&str]) -> usize {
    let mut i = 0;
    while i < lines.len() && js_text::trim(lines[i]).is_empty() {
        i += 1;
    }
    if lines.get(i).map(|l| js_text::trim(l)) != Some("---") {
        return 0;
    }
    i += 1;
    while i < lines.len() && js_text::trim(lines[i]) != "---" {
        i += 1;
    }
    i + 1
}

/// All statements in a source block, in order, a leading frontmatter block skipped.
pub(super) fn statements_of(src: &str) -> Vec<String> {
    let lines = src_lines(src);
    let mut out = Vec::new();
    let start = frontmatter_end(&lines).min(lines.len());
    for line in &lines[start..] {
        split_statements(line, &mut out);
    }
    out
}

/// The `title:` of a leading frontmatter block, if any (the one frontmatter key with
/// terminal meaning).
pub(super) fn frontmatter_title(src: &str) -> Option<String> {
    let lines = src_lines(src);
    let end = frontmatter_end(&lines).min(lines.len());
    for line in &lines[..end] {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // Untrimmed on the left: an indented `title:` is nested under another key.
        if js_text::trim_end(key) != "title" {
            continue;
        }
        let t = js_text::trim(value);
        let quoted = t.chars().count() > 1
            && ((t.starts_with('"') && t.ends_with('"'))
                || (t.starts_with('\'') && t.ends_with('\'')));
        let title = js_text::trim(if quoted { &t[1..t.len() - 1] } else { t });
        return (!title.is_empty()).then(|| title.to_owned());
    }
    None
}

/// Per-char flags: true where the char lies inside a double-quoted span (quotes included).
pub(super) fn quote_mask(chars: &[char]) -> Vec<bool> {
    let mut in_quotes = false;
    chars
        .iter()
        .map(|&c| {
            if c == '"' {
                in_quotes = !in_quotes;
                true
            } else {
                in_quotes
            }
        })
        .collect()
}

/// Split on separator chars outside double quotes and parentheses, dropping empty
/// segments.
pub(super) fn split_top(s: &str, is_sep: impl Fn(char) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut depth = 0usize;
    for c in s.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && c == '(' {
            depth += 1;
        } else if !in_quotes && c == ')' && depth > 0 {
            depth -= 1;
        }
        if !in_quotes && depth == 0 && is_sep(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split `head : rest` at the first label colon, skipping `:::` tag runs so
/// `A:::hot : desc` keeps its tag with the id.
pub(super) fn split_colon(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        let mut run = i;
        while run < bytes.len() && bytes[run] == b':' {
            run += 1;
        }
        if run - i >= 3 {
            i = run;
            continue;
        }
        return Some((&s[..i], &s[i + 1..]));
    }
    None
}

/// Strip trailing `:::name` tags from an id token: `A:::hot` → `A`. (The tags name author
/// classes, which this port does not carry; only the id matters.)
pub(super) fn take_tags(token: &str) -> &str {
    match token.split_once(":::") {
        Some((id, _)) if !id.is_empty() => id,
        _ => token,
    }
}

/// Whether the body of a `class A,B name` statement reads as an assignment: some
/// whitespace separates the ids from the name list.
pub(super) fn is_class_assign(rest: &str) -> bool {
    js_text::has_space(js_text::trim(rest))
}

/// The first whitespace-separated word, or `""`.
pub(super) fn first_word(s: &str) -> &str {
    js_text::words(s).first().copied().unwrap_or("")
}

/// Diagram kind from the header statement, ASCII-lowercased.
pub(super) fn header_kind(statements: &[String]) -> Option<String> {
    let kind = first_word(statements.first()?);
    (!kind.is_empty()).then(|| ascii_lower(kind))
}

/// `None` for the empty string (the package's `nonEmpty`).
pub(super) fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}
