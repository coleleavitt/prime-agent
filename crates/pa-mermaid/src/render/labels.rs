//! Label text: control stripping, entity decoding, markup stripping, wrapping, truncation.
//! Ported from lovely-mermaid 0.3.3 `labels.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).

use super::js_text;
use super::width::{measured, string_width};

/// Node labels wrap to at most this many display columns per line ...
pub(super) const WRAP_WIDTH: usize = 24;
/// ... and at most this many lines; overflow is truncated with an ellipsis.
pub(super) const MAX_LINES: usize = 4;
/// Edge labels are truncated to this many columns.
pub(super) const MAX_LABEL: usize = 28;

/// Identifier-boundary characters preferred as break points inside a too-wide word.
const LABEL_BREAK_CHARS: [char; 4] = ['_', '-', '.', '/'];

/// C0 and C1 controls less `\t\n\r`: they measure a column and paint none, NUL collides
/// with the wide-glyph continuation sentinel, and ESC would inject ANSI.
fn is_stripped_control(c: char) -> bool {
    matches!(c, '\0'..='\x08' | '\x0b' | '\x0c' | '\x0e'..='\x1f' | '\x7f'..='\u{9f}')
}

/// Applied to every untrusted source before parsing.
pub(super) fn strip_controls(src: &str) -> String {
    src.chars().filter(|&c| !is_stripped_control(c)).collect()
}

/// Lines the way Rust's `str::lines()` splits them (the package mirrors it): on `\n`,
/// a trailing `\r` stripped, no final empty line for a trailing newline.
pub(super) fn src_lines(src: &str) -> Vec<&str> {
    let mut out: Vec<&str> = src
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if out.last() == Some(&"") {
        out.pop();
    }
    out
}

/// JS `/[\p{Alphabetic}\p{N}]/u`, which is Rust's `char::is_alphanumeric`.
pub(super) fn is_alphanumeric(c: char) -> bool {
    c.is_alphanumeric()
}

/// Characters allowed in a bare node/state/class identifier.
pub(super) fn is_id_char(c: char) -> bool {
    is_alphanumeric(c) || c == '_'
}

/// The package's ASCII-only lowercasing (Rust's `to_ascii_lowercase`).
pub(super) fn ascii_lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Mermaid writes generics as `List~T~`; show them as `List<T>`.
pub(super) fn display_generics(s: &str) -> String {
    let mut open = false;
    s.chars()
        .map(|c| {
            if c == '~' {
                open = !open;
                if open {
                    '<'
                } else {
                    '>'
                }
            } else {
                c
            }
        })
        .collect()
}

const ENTITY_LOOKAHEAD: usize = 10;

fn decode_entity_body(body: &str) -> Option<char> {
    match body {
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "amp" => return Some('&'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    let num = body.strip_prefix('#')?;
    let (digits, radix) = match num.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (num, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    // At most nine digits fit the lookahead window, so the value cannot overflow.
    let code = u32::from_str_radix(digits, radix).ok()?;
    // Reject control chars: NUL collides with the continuation sentinel and ESC would
    // inject ANSI. Surrogates and out-of-range values are not characters at all.
    if code < 0x20 || (0x7f..=0x9f).contains(&code) {
        return None;
    }
    char::from_u32(code)
}

/// Decode HTML entities in label text in one pass (`&amp;lt;` decodes to `&lt;`).
pub(super) fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let hi = (i + 1 + ENTITY_LOOKAHEAD).min(chars.len());
        let semi = (i + 1..hi).find(|&j| chars[j] == ';');
        let decoded = semi.and_then(|semi| {
            let body: String = chars[i + 1..semi].iter().collect();
            decode_entity_body(&body).map(|c| (c, semi))
        });
        if let Some((c, semi)) = decoded {
            out.push(c);
            i = semi + 1;
        } else {
            out.push('&');
            i += 1;
        }
    }
    out
}

/// Strip markdown emphasis from a `` `backtick` `` label string.
fn strip_markdown(s: &str) -> String {
    let no_code: String = s.chars().filter(|&c| c != '`').collect();
    let no_strong = no_code.replace("**", "").replace("__", "");
    let chars: Vec<char> = no_strong.chars().collect();
    let mut out = String::with_capacity(no_strong.len());
    for (i, &c) in chars.iter().enumerate() {
        // Keep `*`/`_` only inside a word, so snake_case survives.
        let in_word = i > 0
            && is_alphanumeric(chars[i - 1])
            && chars.get(i + 1).is_some_and(|&next| is_alphanumeric(next));
        if (c == '*' || c == '_') && !in_word {
            continue;
        }
        out.push(c);
    }
    js_text::trim(&out).to_owned()
}

/// Inline formatting tags that carry no meaning in a terminal; any other tag-looking text
/// (`Vec<String>`, `<id>`) is left alone.
const HTML_FORMAT_TAGS: [&str; 25] = [
    "b", "strong", "i", "em", "u", "s", "strike", "del", "ins", "mark", "small", "big", "sub",
    "sup", "code", "kbd", "samp", "var", "tt", "span", "font", "q", "abbr", "cite", "pre",
];

/// A tag starting at `start`: its name and the index after `>`.
fn html_tag_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    if chars.get(i) == Some(&'/') {
        i += 1;
    }
    let name_start = i;
    while i < chars.len() && chars[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();
    while i < chars.len() && chars[i] != '>' {
        if chars[i] == '<' {
            return None;
        }
        i += 1;
    }
    (chars.get(i) == Some(&'>')).then_some((name, i + 1))
}

fn strip_html_tags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some((name, end)) = html_tag_at(&chars, i) {
                let lower = name.to_ascii_lowercase();
                if lower == "br" {
                    out.push(' ');
                    i = end;
                    continue;
                }
                if HTML_FORMAT_TAGS.contains(&lower.as_str()) {
                    i = end;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip one matching pair of single-character wrapping delimiters, if present.
fn unwrap(s: &str, delimiter: char) -> Option<&str> {
    (s.len() >= 2 && s.starts_with(delimiter) && s.ends_with(delimiter)).then(|| &s[1..s.len() - 1])
}

/// Normalise raw label text: strip markup, unquote, decode entities (after tag-stripping,
/// so `&lt;b&gt;` survives as the literal `<b>`).
pub(super) fn clean_label(raw: &str) -> String {
    let stripped = strip_html_tags(js_text::trim(raw));
    let trimmed = js_text::trim(&stripped);
    let unquoted = js_text::trim(
        unwrap(trimmed, '"')
            .or_else(|| unwrap(trimmed, '\''))
            .unwrap_or(trimmed),
    );
    match unwrap(unquoted, '`') {
        Some(md) => decode_html_entities(&strip_markdown(js_text::trim(md))),
        None => decode_html_entities(unquoted),
    }
}

/// Byte index of the last identifier-boundary character, if any.
fn last_break(s: &str) -> Option<usize> {
    LABEL_BREAK_CHARS.iter().filter_map(|&c| s.rfind(c)).max()
}

/// Wrap a label to `width` columns over at most `max_lines` lines, truncating the last
/// line with an ellipsis on overflow. A word too wide to fit breaks after the last
/// identifier boundary that fits, else per character.
pub(super) fn wrap_label(label: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;

    for word in js_text::words(label) {
        let ww = string_width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for (ch, cw) in measured(word) {
                if chunk_w + cw > width && !chunk.is_empty() {
                    match last_break(&chunk) {
                        Some(p) => {
                            let carry = chunk[p + 1..].to_owned();
                            chunk.truncate(p + 1);
                            lines.push(std::mem::replace(&mut chunk, carry));
                        }
                        None => lines.push(std::mem::take(&mut chunk)),
                    }
                    chunk_w = string_width(&chunk);
                }
                chunk.push_str(ch);
                chunk_w += cw;
            }
            cur = chunk;
            cur_w = chunk_w;
        } else if cur.is_empty() {
            word.clone_into(&mut cur);
            cur_w = ww;
        } else if cur_w + 1 + ww <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::replace(&mut cur, word.to_owned()));
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let target = width.saturating_sub(1).max(1);
        let mut s = String::new();
        let mut sw = 0;
        let last = lines.last_mut().expect("max_lines rows remain");
        for (ch, cw) in measured(last) {
            if sw + cw > target {
                break;
            }
            s.push_str(ch);
            sw += cw;
        }
        s.push('…');
        *last = s;
    }
    lines
}

/// Truncate to `inner` columns, leaving room for the ellipsis.
pub(super) fn fit_label(label: &str, inner: usize) -> String {
    if string_width(label) <= inner {
        return label.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for (c, cw) in measured(label) {
        if used + cw + 1 > inner {
            break;
        }
        out.push_str(c);
        used += cw;
    }
    out.push('…');
    out
}
