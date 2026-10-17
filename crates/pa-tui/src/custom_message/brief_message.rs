//! The `user_message` row: one `rlm.send_user_message` (Claude Code's
//! `SendUserMessage`, brief mode's "primary visible output channel"). It
//! renders prominently — an accent `● Message` header (tagged when the agent
//! initiated it), the markdown body, then its attachments — like Claude
//! Code's brief message (`p5ctz61b:Hh`: the marker, the message, and
//! `› [image] path (size)` lines).

use serde_json::Value;

use super::geometry::{markdown_style, text_row_count};
use super::render::{spacer, text_rows};
use crate::branch::{branch_markdown, branch_markdown_count};
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// Wire twin of `pa_core::session_engine::brief_mode::USER_MESSAGE_CUSTOM_TYPE`.
pub const USER_MESSAGE_CUSTOM_TYPE: &str = "user_message";

/// One attachment the message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefAttachment {
    pub path: String,
    pub size: u64,
    pub is_image: bool,
}

/// One message the agent sent the user.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefMessageRow {
    /// The markdown body.
    pub message: String,
    /// `status: "proactive"`: the agent initiated it.
    pub proactive: bool,
    pub attachments: Vec<BriefAttachment>,
}

/// Decode a `user_message` row (its text and `details`).
#[must_use]
pub fn brief_message_row(content: &str, details: &Value) -> BriefMessageRow {
    let attachments = details
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|attachment| {
            Some(BriefAttachment {
                path: attachment.get("path")?.as_str()?.to_string(),
                size: attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
                is_image: attachment
                    .get("isImage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect();
    BriefMessageRow {
        message: content.to_string(),
        proactive: details.get("status").and_then(Value::as_str) == Some("proactive"),
        attachments,
    }
}

fn header(row: &BriefMessageRow, theme: &Theme) -> Line {
    let mut spans = vec![
        Span::styled("\u{25cf} ", theme.fg_style(ThemeColor::Accent)),
        Span::styled(
            "Message",
            theme
                .fg_style(ThemeColor::Accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
    ];
    if row.proactive {
        spans.push(Span::styled(
            " \u{b7} proactive",
            theme.fg_style(ThemeColor::Dim),
        ));
    }
    spans
}

/// `1.5 KB`-style sizes.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn attachment_line(attachment: &BriefAttachment, theme: &Theme) -> Line {
    let kind = if attachment.is_image {
        "[image]"
    } else {
        "[file]"
    };
    vec![
        Span::styled(format!("\u{203a} {kind} "), theme.fg_style(ThemeColor::Dim)),
        Span::styled(attachment.path.clone(), theme.fg_style(ThemeColor::Text)),
        Span::styled(
            format!(" ({})", human_size(attachment.size)),
            theme.fg_style(ThemeColor::Dim),
        ),
    ]
}

fn body_style(theme: &Theme) -> crate::markdown::MarkdownStyle {
    markdown_style(ThemeColor::Text, theme)
}

/// The message's rows: a spacer, the header, the body, the attachments.
pub(crate) fn render_brief_message(
    row: &BriefMessageRow,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let mut out = vec![spacer()];
    out.extend(text_rows(&header(row, theme), width));
    out.extend(branch_markdown(
        &row.message,
        &body_style(theme),
        theme,
        width,
    ));
    for attachment in &row.attachments {
        out.extend(text_rows(&attachment_line(attachment, theme), width));
    }
    out
}

/// The row count [`render_brief_message`] produces.
pub(crate) fn brief_message_row_count(row: &BriefMessageRow, theme: &Theme, width: usize) -> usize {
    1 + text_row_count(&header(row, theme), width)
        + branch_markdown_count(&row.message, &body_style(theme), width)
        + row
            .attachments
            .iter()
            .map(|attachment| text_row_count(&attachment_line(attachment, theme), width))
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::theme::ColorMode;

    fn flat(rows: &[Line]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_message_renders_its_header_body_and_attachments() {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let row = brief_message_row(
            "Fixed the **null check** in `auth.rs:42`.",
            &json!({
                "status": "proactive",
                "attachments": [
                    { "path": "/r/shot.png", "size": 2048, "isImage": true },
                    { "path": "/r/diff.txt", "size": 12, "isImage": false },
                    { "size": 1 },
                ],
            }),
        );
        assert_eq!(
            row.attachments,
            vec![
                BriefAttachment {
                    path: "/r/shot.png".to_string(),
                    size: 2048,
                    is_image: true
                },
                BriefAttachment {
                    path: "/r/diff.txt".to_string(),
                    size: 12,
                    is_image: false
                },
            ]
        );
        let rows = render_brief_message(&row, &theme, 80);
        let text = flat(&rows);
        assert_eq!(text[0], "");
        assert_eq!(text[1], " \u{25cf} Message \u{b7} proactive");
        assert!(
            text[2].contains("Fixed the null check in auth.rs:42."),
            "{text:?}"
        );
        assert_eq!(
            text[text.len() - 2],
            " \u{203a} [image] /r/shot.png (2.0 KB)"
        );
        assert_eq!(text[text.len() - 1], " \u{203a} [file] /r/diff.txt (12 B)");
        for width in [20, 40, 80] {
            assert_eq!(
                render_brief_message(&row, &theme, width).len(),
                brief_message_row_count(&row, &theme, width),
                "count matches render at width {width}"
            );
        }
    }
}
