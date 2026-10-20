//! HTML to Markdown (Claude Code runs turndown with `style`, `script`,
//! `noscript` and `iframe` removed, over at most the first MiB of the
//! page).

/// The longest HTML converted, in bytes (Claude Code's cap).
pub const MAX_HTML_BYTES: usize = 1024 * 1024;
/// The marker a capped page ends with.
pub const TRUNCATED_MARKER: &str = "\n\n[Content truncated due to length...]";

/// Tags whose content never reaches the Markdown.
const SKIPPED_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "template", "svg", "canvas", "head",
];

/// Convert `html` to Markdown; a page past [`MAX_HTML_BYTES`] is converted
/// up to that point and marked. A conversion failure keeps the raw HTML,
/// as Claude Code does.
#[must_use]
pub fn to_markdown(html: &str) -> String {
    let capped = html.len() > MAX_HTML_BYTES;
    let mut end = html.len().min(MAX_HTML_BYTES);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .build();
    let mut markdown = converter
        .convert(&html[..end])
        .unwrap_or_else(|_| html[..end].to_string());
    if capped {
        markdown.push_str(TRUNCATED_MARKER);
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_become_markdown_without_scripts_or_styles() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body>\
                    <h1>Title</h1><p>Some <a href=\"https://x.dev/\">link</a> and <code>code</code>.</p>\
                    <script>alert(1)</script><noscript>no js</noscript>\
                    <ul><li>one</li><li>two</li></ul></body></html>";
        assert_eq!(
            to_markdown(html),
            "# Title\n\nSome [link](https://x.dev/) and `code`.\n\n*   one\n*   two"
        );
    }

    #[test]
    fn oversized_pages_are_capped_and_marked() {
        let html = format!("<p>{}</p>", "a".repeat(MAX_HTML_BYTES + 10));
        let markdown = to_markdown(&html);
        assert!(markdown.ends_with(TRUNCATED_MARKER));
        assert!(markdown.len() <= MAX_HTML_BYTES + TRUNCATED_MARKER.len());
    }
}
