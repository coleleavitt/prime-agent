use ratatui::style::Modifier;

use super::MarkdownStyle;
use crate::{Line, Span};
/// Inline rendering: bold, italic, strikethrough, code, links.
#[must_use]
pub fn render_inline(text: &str, style: &MarkdownStyle) -> Line {
    render_inline_tracked(text, style).0
}

/// The same inline render, plus origin tracking: which spans of the returned line
/// carry a link's `(url)` bracket (`url_slots`; the bracket only renders without
/// OSC 8 support) and which are inline-code spans (`code_slots`), in ascending span
/// order. Style-tapering callers (headings, quotes) preserve those spans by origin —
/// a span that merely renders in the same style is not a slot.
#[must_use]
pub(crate) fn render_inline_tracked(
    text: &str,
    style: &MarkdownStyle,
) -> (Line, Vec<usize>, Vec<usize>) {
    render_inline_ctx(text, style, false)
}

/// `in_link` mirrors marked's `lexer.state.inLink`: the gfm bare-url rule is
/// skipped inside a link label's tokens (the angle `autolink` rule is not).
/// Returns the `(url)`-bracket and inline-code span indices, remapped across every
/// recursive extend.
fn render_inline_ctx(
    text: &str,
    style: &MarkdownStyle,
    in_link: bool,
) -> (Line, Vec<usize>, Vec<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let mut inline = InlineRender {
        text,
        style,
        in_link,
        // Byte offset per char index: the autolink rules run on a slice of the original text, so
        // a candidate-heavy line stays linear in its attempts.
        byte_offsets: text.char_indices().map(|(b, _)| b).collect(),
        // The bare-url email alternative only exists when the line carries an
        // `@` at all; the gate keeps the per-position regex attempts rare.
        line_has_at: chars.contains(&'@'),
        chars,
        spans: Vec::new(),
        url_slots: Vec::new(),
        code_slots: Vec::new(),
        buf: String::new(),
        bold: false,
        italic: false,
    };
    let mut i = 0usize;
    while i < inline.chars.len() {
        let c = inline.chars[i];
        // marked's escape rule: a backslash before ASCII punctuation consumes the
        // backslash, so the punctuation never opens a construct.
        if c == '\\'
            && inline
                .chars
                .get(i + 1)
                .is_some_and(char::is_ascii_punctuation)
        {
            inline.buf.push(inline.chars[i + 1]);
            i += 2;
            continue;
        }
        // Each construct returns the index after itself, or `None` when it does not apply here.
        // marked's inline `autolink` (angle form) then `url` (gfm bare links) are the last two
        // inline rules, tried once every other construct failed.
        let next = match c {
            '`' => inline.code_span(i),
            '[' => inline.link(i),
            '*' | '_' => inline.emphasis(i),
            '~' => inline.strikethrough(i),
            _ => None,
        }
        .or_else(|| inline.autolink(i));
        if let Some(next) = next {
            i = next;
        } else {
            inline.buf.push(c);
            i += 1;
        }
    }
    inline.flush();
    let InlineRender {
        mut spans,
        url_slots,
        code_slots,
        ..
    } = inline;
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    (spans, url_slots, code_slots)
}

/// One inline render pass over a line: the output spans with their tracked slots, the pending
/// plain-text run, and the emphasis context the constructs nest in. Each construct method starts
/// at the construct's first char and returns the index after it, or `None` when the text there
/// is not that construct (the char then renders as plain text).
struct InlineRender<'a> {
    text: &'a str,
    style: &'a MarkdownStyle,
    in_link: bool,
    chars: Vec<char>,
    byte_offsets: Vec<usize>,
    line_has_at: bool,
    spans: Vec<Span>,
    url_slots: Vec<usize>,
    code_slots: Vec<usize>,
    buf: String,
    bold: bool,
    italic: bool,
}

impl InlineRender<'_> {
    /// The current emphasis context as a modifier.
    fn emphasis_modifier(&self) -> Modifier {
        let mut modifier = Modifier::empty();
        if self.bold {
            modifier |= self.style.bold;
        }
        if self.italic {
            modifier |= self.style.italic;
        }
        modifier
    }

    /// Close the pending plain-text run as one span in the current emphasis.
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let modifier = self.emphasis_modifier();
            self.spans.push(Span::styled(
                std::mem::take(&mut self.buf),
                self.style.body.add_modifier(modifier),
            ));
        }
    }

    /// Render `inner` as a nested construct with `modifier` applied on top of its own styles.
    fn append_nested(&mut self, inner: &str, in_link: bool, modifier: Modifier) {
        append_inner(
            &mut self.spans,
            &mut self.url_slots,
            &mut self.code_slots,
            render_inline_ctx(inner, self.style, in_link),
            modifier,
        );
    }

    /// The `(url)` bracket after a link label: without OSC 8 support the URL rides after the label
    /// (the OSC 8 region covers the label only), in the dim `link_url` slot unless the label
    /// already is the URL (mailto stripped for the comparison). A hyperlink-capable terminal shows
    /// the clickable label alone. The bracket renders the destination as visible text, so it gets
    /// the same control-byte hardening as the OSC 8 target: a smuggled escape byte can never
    /// re-enter the terminal live.
    fn push_url_bracket(&mut self, label: &str, url: &str) {
        let comparison = url.strip_prefix("mailto:").unwrap_or(url);
        if !crate::hyperlinks::hyperlinks_enabled() && label != url && label != comparison {
            let shown = crate::hyperlinks::sanitize_control_bytes(url.to_string());
            self.url_slots.push(self.spans.len());
            self.spans
                .push(Span::styled(format!(" ({shown})"), self.style.link_url));
        }
    }

    /// Inline code: `` `code` ``, rendered verbatim in the code style.
    fn code_span(&mut self, i: usize) -> Option<usize> {
        let close = (i + 1..self.chars.len()).find(|&j| self.chars[j] == '`')?;
        let code: String = self.chars[i + 1..close].iter().collect();
        self.flush();
        self.code_slots.push(self.spans.len());
        self.spans.push(Span::styled(code, self.style.code));
        Some(close + 1)
    }

    /// A link `[label](url)`, or an image `![alt](url)`.
    fn link(&mut self, i: usize) -> Option<usize> {
        let close = (i + 1..self.chars.len()).find(|&j| self.chars[j] == ']')?;
        if self.chars.get(close + 1) != Some(&'(') {
            return None;
        }
        let (url, end) = link_destination(&self.chars, close + 2)?;
        let label: String = self.chars[i + 1..close].iter().collect();
        let modifier = self.emphasis_modifier();
        // An image (`![alt](url)`): marked's image token falls through to the
        // alt text in the TS renderer — no `!`, no link affordance, no bracket.
        // The marker is read from the SOURCE: an escaped `\!` is a literal
        // bang, not an image marker (an odd backslash run escapes it).
        if image_marker_before(&self.chars, i) {
            self.buf.pop();
            self.flush();
            self.spans
                .push(Span::styled(label, self.style.body.add_modifier(modifier)));
            return Some(end + 1);
        }
        self.flush();
        // The label renders underlined (the standard terminal link affordance); `modifier`
        // carries the emphasis context.
        let href =
            crate::hyperlinks::resolve_link_href_with_base(&url, self.style.link_base_url.as_ref());
        let (mut label_spans, label_slots, label_codes) =
            render_inline_ctx(&label, self.style, true);
        // OSC 8: the label stays clickable (TS `hyperlink()`).
        let open = crate::hyperlinks::osc8_open(&href);
        if let Some(first) = label_spans.first_mut() {
            first.content.insert_str(0, &open);
        }
        if let Some(last) = label_spans.last_mut() {
            last.content.push_str(crate::hyperlinks::OSC8_CLOSE);
        }
        append_inner(
            &mut self.spans,
            &mut self.url_slots,
            &mut self.code_slots,
            (label_spans, label_slots, label_codes),
            modifier | Modifier::UNDERLINED,
        );
        self.push_url_bracket(&label, &url);
        Some(end + 1)
    }

    /// Emphasis: `*em*`/`_em_`, `**strong**`/`__strong__`, and `***both***`.
    fn emphasis(&mut self, i: usize) -> Option<usize> {
        let bytes = &self.chars;
        let c = bytes[i];
        if i + 1 >= bytes.len() {
            return None;
        }
        // CommonMark flanking for `_`: a delimiter between word characters
        // can neither open nor close emphasis (snake_case prose keeps its
        // underscores); `*` has no intraword restriction.
        if c == '_' && !underscore_can_open(bytes, i) {
            self.buf.push(c);
            return Some(i + 1);
        }
        let is_triple = i + 2 < bytes.len() && bytes[i + 1] == c && bytes[i + 2] == c;
        if is_triple {
            if let Some(close) = find_closing(bytes, i + 3, c, 3) {
                let inner: String = bytes[i + 3..close].iter().collect();
                self.flush();
                self.bold = !self.bold;
                self.italic = !self.italic;
                self.append_nested(&inner, self.in_link, self.style.bold | self.style.italic);
                self.bold = !self.bold;
                self.italic = !self.italic;
                return Some(close + 3);
            }
        }
        let doubled = bytes[i + 1] == c;
        let (len, close_search) = if doubled { (2, i + 2) } else { (1, i + 1) };
        // An intraword `_` cannot close, but the opener still matches a
        // later one — keep searching instead of giving up.
        let mut close = find_closing(bytes, close_search, c, len);
        while close.is_some_and(|k| c == '_' && !underscore_can_close(bytes, k, len)) {
            let resume = close.expect("checked Some") + len;
            close = find_closing(bytes, resume, c, len);
        }
        let close = close?;
        if doubled {
            if let Some(next) = self.strong_with_nested_closer(c, close_search, close) {
                return Some(next);
            }
        }
        let inner: String = self.chars[close_search..close].iter().collect();
        if inner.trim().is_empty() {
            self.buf.push(c);
            return Some(i + 1);
        }
        self.flush();
        if doubled {
            self.bold = !self.bold;
            self.append_nested(&inner, self.in_link, self.style.bold);
            self.bold = !self.bold;
        } else {
            self.italic = !self.italic;
            self.append_nested(&inner, self.in_link, self.style.italic);
            self.italic = !self.italic;
        }
        Some(close + len)
    }

    /// A strong span whose closer run is shared with an unmatched inner single opener: a closer
    /// run of three (`**outer *inner***`) gives the extra star to the inner delimiter, the outer
    /// keeps the last two — marked renders bold(outer + italic(inner)); a run of two with a
    /// non-right-flanking inner opener pairs that opener with the first closer instead. `None`
    /// when the plain strong pairing applies.
    fn strong_with_nested_closer(
        &mut self,
        c: char,
        close_search: usize,
        close: usize,
    ) -> Option<usize> {
        let run = run_length(&self.chars, close, c);
        let (nested_opener, nested_opener_right_flanking) =
            unmatched_inner_opener(&self.chars, c, close_search, close);
        if !nested_opener {
            return None;
        }
        if run == 3 {
            let inner: String = self.chars[close_search..=close].iter().collect();
            if !inner.trim().is_empty() {
                self.flush();
                self.bold = !self.bold;
                self.append_nested(&inner, self.in_link, self.style.bold);
                self.bold = !self.bold;
                return Some(close + 3);
            }
        }
        if run == 2 && !nested_opener_right_flanking {
            // Marked pairs an unmatched inner single opener with the
            // first closer: the outside becomes literal '*' + em,
            // not strong. Actual TS: **a\**b** => '*' + italic(a*b).
            self.buf.push(c);
            self.flush();
            let inner: String = self.chars[close_search..=close].iter().collect();
            self.append_nested(&inner, self.in_link, self.style.italic);
            return Some(close + 2);
        }
        None
    }

    /// Strikethrough: `~~struck~~`.
    fn strikethrough(&mut self, i: usize) -> Option<usize> {
        if self.chars.get(i + 1) != Some(&'~') {
            return None;
        }
        let close = find_closing(&self.chars, i + 2, '~', 2)?;
        let inner: String = self.chars[i + 2..close].iter().collect();
        if inner.trim().is_empty() {
            return None;
        }
        self.flush();
        self.append_nested(&inner, self.in_link, self.style.strikethrough);
        Some(close + 2)
    }

    /// An autolink: the angle form (`<url>`), or a gfm bare url outside a link label. The two
    /// rules are disjoint on their first character.
    fn autolink(&mut self, i: usize) -> Option<usize> {
        let rest = &self.text[self.byte_offsets[i]..];
        let token = if self.chars[i] == '<' {
            crate::autolink::angle_token(rest)
        } else if !self.in_link && crate::autolink::bare_candidate(&self.chars, i, self.line_has_at)
        {
            crate::autolink::bare_token(rest)
        } else {
            None
        }?;
        self.flush();
        // The token is one plain text token, so the label is a single body-colored run carrying
        // the current emphasis (theme.link never reaches the wire). Every link render
        // underlines, both capability forms.
        let mut label = Span::styled(
            token.text.clone(),
            self.style
                .body
                .add_modifier(self.emphasis_modifier() | Modifier::UNDERLINED),
        );
        let href = crate::hyperlinks::resolve_link_href_with_base(
            &token.href,
            self.style.link_base_url.as_ref(),
        );
        label
            .content
            .insert_str(0, &crate::hyperlinks::osc8_open(&href));
        label.content.push_str(crate::hyperlinks::OSC8_CLOSE);
        self.spans.push(label);
        // The bare-url regex tail only excludes whitespace, so the bracket gets the same
        // hardening as the OSC 8 target.
        self.push_url_bracket(&token.text, &token.href);
        Some(i + token.raw.chars().count())
    }
}

/// Scan a strong span's inner text (`from..close`) for an inner single `c` opener that no inner
/// closer matched: `(unmatched, right_flanking)`. A completed `*inner*` pair (or a star inside
/// code) cannot steal the opener of the following adjacent emphasis span.
fn unmatched_inner_opener(bytes: &[char], c: char, from: usize, close: usize) -> (bool, bool) {
    let mut nested_opener = false;
    let mut nested_opener_right_flanking = false;
    let mut at = from;
    while at < close {
        if bytes[at] == '\\' && bytes.get(at + 1).is_some_and(char::is_ascii_punctuation) {
            at += 2;
            continue;
        }
        if bytes[at] == '`' {
            if let Some(end) = (at + 1..close).find(|&k| bytes[k] == '`') {
                at = end + 1;
                continue;
            }
        }
        if bytes[at] != c {
            at += 1;
            continue;
        }
        let length = run_length(bytes, at, c);
        if length == 1 {
            let can_open = bytes.get(at + 1).is_some_and(|next| !next.is_whitespace())
                && (c != '_' || underscore_can_open(bytes, at));
            let can_close = at > 0
                && !bytes[at - 1].is_whitespace()
                && (!bytes[at - 1].is_ascii_punctuation()
                    || bytes
                        .get(at + 1)
                        .is_none_or(|next| next.is_whitespace() || next.is_ascii_punctuation()))
                && (c != '_' || underscore_can_close(bytes, at, 1));
            if nested_opener && can_close {
                nested_opener = false;
            } else if !nested_opener && can_open {
                nested_opener = true;
                nested_opener_right_flanking = can_close;
            }
        }
        at += length;
    }
    (nested_opener, nested_opener_right_flanking)
}

/// True when the `!` immediately before the `[` at `at` is an image marker:
/// an even run of backslashes precedes it (an escaped `!` is a literal bang).
fn image_marker_before(bytes: &[char], at: usize) -> bool {
    if at == 0 || bytes[at - 1] != '!' {
        return false;
    }
    let mut backslashes = 0usize;
    let mut j = at - 1;
    while j > 0 && bytes[j - 1] == '\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes.is_multiple_of(2)
}

/// Recurse into one inline construct's inner text and merge the result into the
/// outer span stream: the tracked slots remap across the extend, and the
/// construct's own modifier applies on top of the inner styles.
fn append_inner(
    outer: &mut Vec<Span>,
    url_slots: &mut Vec<usize>,
    code_slots: &mut Vec<usize>,
    inner: (Line, Vec<usize>, Vec<usize>),
    modifier: Modifier,
) {
    let (mut spans, mut slots, mut codes) = inner;
    let offset = outer.len();
    for slot in &mut slots {
        *slot += offset;
    }
    for slot in &mut codes {
        *slot += offset;
    }
    url_slots.append(&mut slots);
    code_slots.append(&mut codes);
    for s in &mut spans {
        s.style = s.style.add_modifier(modifier);
    }
    outer.extend(spans);
}

/// The `CommonMark` inline-link destination after `[label](` (marked's link rule):
/// parens ride as a balanced pair (the destination ends at its own `)`, not the
/// first one inside), escaped punctuation consumes its backslash, and the destination stops at
/// whitespace — a quoted title may follow before the closing paren. Returns the
/// destination and the index of the closing `)`.
fn link_destination(bytes: &[char], from: usize) -> Option<(String, usize)> {
    let mut k = from;
    while k < bytes.len() && (bytes[k] == ' ' || bytes[k] == '\t') {
        k += 1;
    }
    let mut url = String::new();
    let mut paren_depth = 0usize;
    while k < bytes.len() {
        let b = bytes[k];
        if b == ')' && paren_depth == 0 {
            return Some((url, k));
        }
        if b == ' ' || b == '\t' {
            // Pinned TS/marked accepts an unmatched '(' before a quoted title;
            // its actual frame still links foo(bar in [t](foo(bar "title").
            // Preserve that product behavior rather than imposing a stricter parser.
            break;
        }
        if b == '\\' && bytes.get(k + 1).is_some_and(char::is_ascii_punctuation) {
            k += 1;
            url.push(bytes[k]);
            k += 1;
            continue;
        }
        if b == '(' {
            paren_depth += 1;
        } else if b == ')' {
            paren_depth = paren_depth.saturating_sub(1);
        }
        url.push(b);
        k += 1;
    }
    while k < bytes.len() && (bytes[k] == ' ' || bytes[k] == '\t') {
        k += 1;
    }
    if k < bytes.len() && (bytes[k] == '"' || bytes[k] == '\'') {
        let quote = bytes[k];
        k += 1;
        while k < bytes.len() && bytes[k] != quote {
            // An escaped quote belongs to the title, not its closing delimiter.
            k += if bytes[k] == '\\' && k + 1 < bytes.len() {
                2
            } else {
                1
            };
        }
        if k == bytes.len() {
            return None;
        }
        k += 1;
        while k < bytes.len() && (bytes[k] == ' ' || bytes[k] == '\t') {
            k += 1;
        }
    }
    (k < bytes.len() && bytes[k] == ')').then_some((url, k))
}

/// `CommonMark` flanking for `_` openers: left-flanking and not intraword (the
/// preceding char is not a word character), so `snake_case` never opens.
fn underscore_can_open(bytes: &[char], at: usize) -> bool {
    let next = bytes.get(at + 1);
    let next_ws = next.is_none_or(|c: &char| c.is_whitespace());
    let prev_word = at > 0 && (bytes[at - 1].is_alphanumeric() || bytes[at - 1] == '_');
    !next_ws && !prev_word
}

/// The `_` closer mirror: right-flanking and not intraword (the char after the
/// closer run is not a word character).
fn underscore_can_close(bytes: &[char], at: usize, len: usize) -> bool {
    let prev = at > 0 && !bytes[at - 1].is_whitespace();
    let next_word = bytes
        .get(at + len)
        .is_some_and(|c| c.is_alphanumeric() || *c == '_');
    prev && !next_word
}

/// The length of the run of `delim` starting at `at`.
fn run_length(bytes: &[char], at: usize, delim: char) -> usize {
    let mut n = 0;
    while bytes.get(at + n) == Some(&delim) {
        n += 1;
    }
    n
}

fn find_closing(chars: &[char], from: usize, delim: char, len: usize) -> Option<usize> {
    let mut i = from;
    while i + len <= chars.len() {
        // Escaped punctuation is literal, including a would-be closer. Walking
        // escape pairs also leaves a delimiter after an even backslash run live.
        if chars[i] == '\\' && chars.get(i + 1).is_some_and(char::is_ascii_punctuation) {
            i += 2;
            continue;
        }
        if (0..len).all(|k| chars[i + k] == delim) {
            return Some(i);
        }
        i += 1;
    }
    None
}
