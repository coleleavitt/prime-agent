//! Custom-message row rendering: each component's row geometry and theme
//! colors. The refinement components live in the sibling `refinement`
//! module; compaction outcomes render through the chat status rows.

use super::{
    AgentMessageDirection, AgentMessageRow, CustomPanelRow, ShellCompletionRow, AGENT_MESSAGE_LABEL,
};
use crate::chat::Detail;
use crate::markdown::MermaidMode;
use crate::theme::{Theme, ThemeColor};
use crate::width::{pad_line, str_width, truncate_line, wrap_line, wrap_text};
use crate::{Line, Span};
use ratatui::style::Style;

pub(crate) fn spacer() -> Line {
    Vec::new()
}

/// The bold `[<name>]` label shared by the skill card and the generic
/// custom panel.
pub(crate) fn custom_message_label(name: &str, theme: &Theme) -> Span {
    Span::styled(
        format!("[{name}]"),
        theme
            .fg_style(ThemeColor::CustomMessageLabel)
            .add_modifier(ratatui::style::Modifier::BOLD),
    )
}

/// A `Text(spans, 1, 0)` row set: content wrapped at `width - 2`, one margin
/// column, padded to the full width with the default style.
pub(crate) fn text_rows(spans: &Line, width: usize) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let flat: String = spans.iter().map(|s| s.content.as_str()).collect();
    if flat.trim().is_empty() {
        return Vec::new();
    }
    wrap_line(spans, content_width)
        .into_iter()
        .map(|wrapped| {
            let mut row: Line = vec![Span::raw(" ")];
            row.extend(wrapped);
            pad_line(row, width)
        })
        .collect()
}

/// The `✉ <label> · <participant>` summary line, with sanctioned divergences from TS:
/// the `✉` mail envelope (Kevin directive 2026-09-24) renders green (operator directive
/// 2026-09-24), and the participant is the viewer-relative arrow plus the counterpart's
/// name (operator directive 2026-09-25).
pub(crate) fn agent_message_summary_line(
    direction: AgentMessageDirection,
    counterpart: &str,
    theme: &Theme,
) -> Line {
    let arrow = match direction {
        AgentMessageDirection::Received => "\u{2193}",
        AgentMessageDirection::Sent | AgentMessageDirection::Queued => "\u{2191}",
    };
    vec![
        Span::styled("\u{2709}".to_string(), theme.fg_style(ThemeColor::Success)),
        Span::raw(" "),
        Span::styled(
            AGENT_MESSAGE_LABEL.to_string(),
            theme.fg_style(ThemeColor::Muted),
        ),
        Span::styled(" \u{b7} ".to_string(), theme.fg_style(ThemeColor::Dim)),
        Span::styled(
            format!("{arrow} {counterpart}"),
            theme.fg_style(ThemeColor::Dim),
        ),
    ]
}

/// The received agent-message rows: a leading blank (spacing-driven), the summary header
/// (no body preview), and the `╰─`-guttered body when expanded.
pub(crate) fn render_agent_message(
    row: &AgentMessageRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
    mermaid: MermaidMode,
) -> Vec<Line> {
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    let header = agent_message_summary_line(row.direction, &row.counterpart, theme);
    out.extend(text_rows(&header, width));
    if detail.tool_output_expanded() {
        out.extend(agent_message_body(&row.message, theme, width, mermaid));
    }
    out
}

/// The body rows of a message an installed diagram renderer draws into (TS
/// `createMermaidTextRenderer`): prose re-wrapped at the body width, drawn rows as they
/// are, notices in their level's color. `None` when no renderer draws agent messages, the
/// mode is off, or nothing was drawn — the body keeps its plain rendering.
pub(crate) fn agent_body_diagram_rows(
    message: &str,
    theme: &Theme,
    width: usize,
    mermaid: MermaidMode,
) -> Option<Vec<Line>> {
    if !crate::diagram::mode_active(mermaid, false) {
        return None;
    }
    let renderer = crate::diagram::installed()
        .filter(|renderer| renderer.draws_on(crate::diagram::DiagramSurface::AgentMessage))?;
    let text_width = super::geometry::agent_body_width(width);
    let segments = crate::diagram::text_segments(message, text_width, renderer)?;
    let palette = crate::markdown::MermaidPalette::from_theme(theme);
    let body = theme.fg_style(ThemeColor::CustomMessageText);
    let mut lines: Vec<Line> = Vec::new();
    for segment in segments {
        match segment {
            crate::diagram::TextSegment::Rows(rows) => lines.extend(
                rows.iter()
                    .map(|row| crate::markdown::drawn_spans(row, &palette, |_| body)),
            ),
            crate::diagram::TextSegment::Text(text_lines) => {
                for fragments in text_lines {
                    let line: Line = fragments
                        .into_iter()
                        .map(|(text, level)| {
                            let style = level.map_or(body, |level| {
                                crate::markdown::notice_style(level, &palette)
                            });
                            Span::styled(text, style)
                        })
                        .collect();
                    let wrapped = wrap_line(&line, text_width);
                    if wrapped.is_empty() {
                        lines.push(Vec::new());
                    }
                    lines.extend(wrapped);
                }
            }
        }
    }
    Some(lines)
}

/// Each source line wraps at `width - 4`: the first rendered line carries
/// the `╰─ ` gutter, the rest three spaces, all in `customMessageText` (diagram rows and
/// notices an installed renderer draws keep their own colors).
pub(crate) fn agent_message_body(
    message: &str,
    theme: &Theme,
    width: usize,
    mermaid: MermaidMode,
) -> Vec<Line> {
    let safe_width = width.max(1);
    let text_width = super::geometry::agent_body_width(width);
    let body = theme.fg_style(ThemeColor::CustomMessageText);
    let lines = agent_body_diagram_rows(message, theme, width, mermaid).unwrap_or_else(|| {
        let mut lines: Vec<Line> = Vec::new();
        for source in message.split('\n') {
            for line in wrap_text(source, text_width) {
                lines.push(
                    line.into_iter()
                        .map(|span| Span::styled(span.content, body))
                        .collect(),
                );
            }
        }
        lines
    });
    let mut lines = lines;
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    let dim = theme.fg_style(ThemeColor::Dim);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            // The first rendered line carries the dim `╰─ ` gutter,
            // continuation lines three unstyled spaces.
            let mut row: Line = vec![Span::raw(" ")];
            if index == 0 {
                row.push(Span::styled("\u{2570}\u{2500} ".to_string(), dim));
            } else {
                row.push(Span::raw("   "));
            }
            row.extend(line);
            truncate_line(&row, safe_width, "")
        })
        .collect()
}

/// Plain-text truncate with an explicit ellipsis.
pub(crate) fn truncate_text(text: &str, width: usize, ellipsis: &str) -> String {
    let line: Line = vec![Span::raw(text.to_string())];
    let truncated = truncate_line(&line, width, ellipsis);
    truncated
        .iter()
        .map(|s| s.content.as_str())
        .collect::<String>()
}

/// One shell-completion row, standalone form: the header mark, then the
/// raw content under the branch gutter when expanded.
pub(crate) fn render_shell_completion(
    row: &ShellCompletionRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
) -> Vec<Line> {
    let failed = matches!(row.exit_code, Some(code) if code != 0);
    let color = if failed {
        theme.fg_style(ThemeColor::Error)
    } else {
        theme.fg_style(ThemeColor::Muted)
    };
    let label = if let (Some(code), true) = (row.exit_code, failed) {
        format!("Background shell command failed \u{b7} exit {code}")
    } else {
        "Background shell command finished".to_string()
    };
    let mark = if failed { "\u{2717}" } else { "\u{2713}" };
    let header = truncate_line(
        &vec![Span::styled(format!(" {mark} {label}"), color)],
        width,
        "",
    );
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    out.push(header);
    if detail.tool_output_expanded() {
        out.extend(crate::branch::branch_block(
            &vec![Span::raw(row.content.clone())],
            theme,
            width,
        ));
    }
    out
}

/// Pad a rendered line to the full width with a base style.
pub(crate) fn pad_with(mut line: Line, width: usize, base: Style) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    if used < width {
        line.push(Span::styled(" ".repeat(width - used), base));
    }
    line
}

/// One generic custom row: a leading blank, the bold `[<customType>]` label, then the
/// always-shown markdown body in `customMessageText` under the branch gutter.
pub(crate) fn render_custom_panel(
    row: &CustomPanelRow,
    theme: &Theme,
    width: usize,
    mermaid: MermaidMode,
) -> Vec<Line> {
    let mut md = super::geometry::markdown_style(ThemeColor::CustomMessageText, theme);
    md.mermaid = crate::diagram::surface_render(
        crate::diagram::DiagramSurface::CustomMessage,
        mermaid,
        false,
    );
    let mut out = vec![spacer()];
    out.extend(text_rows(
        &vec![custom_message_label(&row.custom_type, theme)],
        width,
    ));
    // A preview the terminal can place: the label, then the reserved rows
    // the image goes over after the frame flush.
    let placed = row.image.as_deref().and_then(|preview| {
        crate::inline_image::image_block(&preview.image, width).map(|block| (preview, block))
    });
    if let Some((preview, block)) = placed {
        out.extend(crate::branch::branch_markdown(
            &preview.label,
            &md,
            theme,
            width,
        ));
        out.extend(crate::inline_image::image_block_rows(&preview.image, block));
        return out;
    }
    out.extend(crate::branch::branch_markdown(
        &row.content,
        &md,
        theme,
        width,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Detail;
    use crate::theme::{ColorMode, Theme};
    use crate::Span;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn flat(row: &Line) -> String {
        row.iter().map(|s| s.content.as_str()).collect()
    }

    #[test]
    fn agent_message_header_shape() {
        let row = AgentMessageRow {
            direction: AgentMessageDirection::Received,
            counterpart: "model-probe".to_string(),
            message: "ready".to_string(),
        };
        let rows = render_agent_message(
            &row,
            Detail::Overview,
            &theme(),
            60,
            true,
            MermaidMode::default(),
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].is_empty());
        let header = flat(&rows[1]);
        assert_eq!(
            header.trim_end(),
            " \u{2709} Agent message \u{b7} \u{2193} model-probe"
        );
        assert!(!header.contains("ready"), "no preview: {header:?}");
        // Colors: green envelope, muted label, dim viewer-relative arrow
        // plus name.
        let green = theme().fg_style(ThemeColor::Success);
        let muted = theme().fg_style(ThemeColor::Muted);
        let dim = theme().fg_style(ThemeColor::Dim);
        assert_eq!(rows[1][0], Span::styled(" ".to_string(), Style::default()));
        assert_eq!(rows[1][1], Span::styled("\u{2709}".to_string(), green));
        assert_eq!(rows[1][3], Span::styled("Agent message".to_string(), muted));
        assert_eq!(rows[1][4], Span::styled(" \u{b7} ".to_string(), dim));
        assert_eq!(
            rows[1][5],
            Span::styled("\u{2193} model-probe".to_string(), dim)
        );
    }

    #[test]
    fn agent_message_header_never_carries_the_body() {
        // An empty body and a long body render the SAME collapsed header.
        for message in ["  \n  ".to_string(), format!("{} end", "word ".repeat(20))] {
            let row = AgentMessageRow {
                direction: AgentMessageDirection::Received,
                counterpart: "root".to_string(),
                message,
            };
            let rows = render_agent_message(
                &row,
                Detail::Overview,
                &theme(),
                60,
                false,
                MermaidMode::default(),
            );
            assert_eq!(rows.len(), 1, "one header row: {rows:?}");
            let header = flat(&rows[0]).trim_end().to_string();
            assert_eq!(header, " \u{2709} Agent message \u{b7} \u{2193} root");
            assert!(!header.contains("word"), "no preview: {header:?}");
            assert!(!header.contains("\u{2026}"), "no ellipsis: {header:?}");
        }
    }

    /// The arrow comes from the row's actual direction, never parsed out of a string.
    #[test]
    fn agent_message_arrows_follow_the_row_direction() {
        for (direction, arrow) in [
            (AgentMessageDirection::Received, "\u{2193}"),
            (AgentMessageDirection::Sent, "\u{2191}"),
            (AgentMessageDirection::Queued, "\u{2191}"),
        ] {
            let row = AgentMessageRow {
                direction,
                counterpart: "worker".to_string(),
                message: "ping".to_string(),
            };
            let rows = render_agent_message(
                &row,
                Detail::Overview,
                &theme(),
                80,
                false,
                MermaidMode::default(),
            );
            let header = flat(&rows[0]);
            assert!(
                header.contains(&format!("\u{2709} Agent message \u{b7} {arrow} worker")),
                "{direction:?} header: {header}"
            );
            assert!(
                !header.contains("ping"),
                "the body never leaks into the collapsed row: {header}"
            );
        }
    }

    #[test]
    fn agent_message_body_gutter_when_expanded() {
        let row = AgentMessageRow {
            direction: AgentMessageDirection::Received,
            counterpart: "root".to_string(),
            message: "line one\nline two".to_string(),
        };
        let rows = render_agent_message(
            &row,
            Detail::All,
            &theme(),
            60,
            false,
            MermaidMode::default(),
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(flat(&rows[1]), " \u{2570}\u{2500} line one");
        assert_eq!(flat(&rows[2]), "    line two");
        let dim = theme().fg_style(ThemeColor::Dim);
        let body = theme().fg_style(ThemeColor::CustomMessageText);
        // The first rendered line carries the dim gutter, continuation
        // rows three unstyled spaces.
        assert_eq!(
            rows[1][1],
            Span::styled("\u{2570}\u{2500} ".to_string(), dim)
        );
        assert_eq!(rows[2][1], Span::raw("   "));
        assert!(rows[1]
            .iter()
            .any(|span| span.content == "line one" && span.style == body));
    }

    /// No diagram renderer is installed in this binary: agent-message bodies and custom
    /// panels keep their `mermaid` fences as text, as the native product renders them.
    #[test]
    fn without_an_installed_renderer_message_bodies_keep_their_fences() {
        let fence = "Plan:\n```mermaid\nflowchart LR\n  A --> B\n```";
        let row = AgentMessageRow {
            direction: AgentMessageDirection::Received,
            counterpart: "lane".to_string(),
            message: fence.to_string(),
        };
        let rows = render_agent_message(
            &row,
            Detail::All,
            &theme(),
            60,
            false,
            MermaidMode::Streaming,
        );
        let text: Vec<String> = rows
            .iter()
            .map(|r| flat(r).trim_end().to_string())
            .collect();
        assert_eq!(
            text[1..],
            [
                " ╰─ Plan:",
                "    ```mermaid",
                "    flowchart LR",
                "      A --> B",
                "    ```",
            ]
        );
        assert_eq!(
            crate::custom_message::geometry::agent_message_row_count(
                &row,
                Detail::All,
                &theme(),
                60,
                false,
                MermaidMode::Streaming,
            ),
            rows.len()
        );
        let panel = CustomPanelRow {
            custom_type: "note".to_string(),
            content: fence.to_string(),
            image: None,
        };
        let panel_text: Vec<String> =
            render_custom_panel(&panel, &theme(), 60, MermaidMode::Streaming)
                .iter()
                .map(|r| flat(r).trim_end().to_string())
                .collect();
        assert!(
            panel_text.iter().any(|r| r.ends_with("flowchart LR")),
            "{panel_text:?}"
        );
    }

    #[test]
    fn shell_completion_rows() {
        let ok = ShellCompletionRow {
            pid: Some(4371),
            exit_code: Some(0),
            content: "[bash-done pid:4371 exit:0]".to_string(),
        };
        let rows = render_shell_completion(&ok, Detail::Overview, &theme(), 60, true);
        assert!(rows[0].is_empty());
        assert_eq!(
            flat(&rows[1]),
            " \u{2713} Background shell command finished"
        );
        assert_eq!(
            rows[1][0].style,
            theme().fg_style(ThemeColor::Muted),
            "muted when exit 0"
        );
        let failed = ShellCompletionRow {
            pid: Some(11),
            exit_code: Some(2),
            content: "[bash-done pid:11 exit:2]".to_string(),
        };
        let rows = render_shell_completion(&failed, Detail::Overview, &theme(), 60, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            flat(&rows[0]),
            " \u{2717} Background shell command failed \u{b7} exit 2"
        );
        assert_eq!(
            rows[0][0].style,
            theme().fg_style(ThemeColor::Error),
            "error when failed"
        );
        // The expanded body is the raw content under the branch gutter: the gutter on
        // the first content row, the four-column continuation on the blank source line.
        let row = ShellCompletionRow {
            pid: Some(99),
            exit_code: Some(0),
            content: "[bash-done pid:99 exit:0]\n\nCommand: \"printf done\"".to_string(),
        };
        let rows = render_shell_completion(&row, Detail::All, &theme(), 60, true);
        let trimmed: Vec<String> = rows
            .iter()
            .map(|row| flat(row).trim_end().to_string())
            .collect();
        assert_eq!(
            trimmed,
            vec![
                "",
                " \u{2713} Background shell command finished",
                " \u{2570}\u{2500} [bash-done pid:99 exit:0]",
                "",
                "    Command: \"printf done\"",
            ],
            "{rows:?}"
        );
    }

    /// A presented artifact's preview row as the TUI decodes it.
    fn presented_preview_row() -> CustomPanelRow {
        let entries = crate::custom_message::custom_message_entries(&serde_json::json!({
            "role": "custom",
            "customType": crate::custom_message::PRESENTED_ARTIFACT_CUSTOM_TYPE,
            "content": [
                { "type": "text", "text": "Direction A" },
                { "type": "image", "data": "aGk=", "mimeType": "image/png" }
            ],
            "display": true,
            "details": { "name": "render.png", "kind": "image", "mimeType": "image/png",
                         "width": 1600, "height": 900 },
        }));
        let [crate::chat::ChatEntry::CustomPanel(row)] = entries.as_slice() else {
            panic!("one panel: {entries:?}");
        };
        (**row).clone()
    }

    /// Upstream #1062 follow-up: on a terminal that places images, the
    /// presented artifact's preview is a reserved image block under its
    /// label (it was the `[Image: …]` text panel everywhere); the geometry
    /// counts the same rows.
    #[test]
    fn a_presented_preview_reserves_its_image_rows_where_images_place() {
        use crate::terminal_image::{
            clear_image_protocol_override, set_cell_dimensions_override,
            set_image_protocol_override, CellDimensions, ImageProtocol,
        };
        let row = presented_preview_row();
        let text = |rows: &[Line]| -> Vec<String> {
            rows.iter()
                .map(|line| {
                    let mut line = line.clone();
                    crate::inline_image::strip_markers(&mut line);
                    flat(&line).trim_end().to_string()
                })
                .collect()
        };
        set_cell_dimensions_override(Some(CellDimensions {
            width_px: 10,
            height_px: 20,
        }));
        set_image_protocol_override(Some(ImageProtocol::Kitty));
        let placed = render_custom_panel(&row, &theme(), 80, MermaidMode::default());
        let placed_count = crate::custom_message::geometry::custom_panel_row_count(
            &row,
            &theme(),
            80,
            MermaidMode::default(),
        );
        set_image_protocol_override(None);
        let fallback = render_custom_panel(&row, &theme(), 80, MermaidMode::default());
        let fallback_count = crate::custom_message::geometry::custom_panel_row_count(
            &row,
            &theme(),
            80,
            MermaidMode::default(),
        );
        clear_image_protocol_override();
        set_cell_dimensions_override(None);
        // 60 columns of a 1600x900 preview at 10x20 px cells: 17 rows.
        let mut expected = vec![
            String::new(),
            " [artifact]".to_string(),
            " \u{2570}\u{2500} Direction A".to_string(),
        ];
        expected.extend(std::iter::repeat_n(String::new(), 17));
        assert_eq!(text(&placed), expected);
        assert_eq!(placed_count, placed.len());
        let markers: Vec<_> = placed[3..]
            .iter()
            .map(|line| {
                line.iter()
                    .find_map(|span| crate::inline_image::parse_marker(&span.content))
                    .map(|tag| (tag.index, tag.rows, tag.column, tag.columns))
            })
            .collect();
        assert_eq!(
            markers,
            (0..17)
                .map(|index| Some((index, 17, 4, 60)))
                .collect::<Vec<_>>()
        );
        // No protocol: the textual fallback panel, unchanged.
        assert_eq!(
            text(&fallback),
            vec![
                "",
                " [artifact]",
                " \u{2570}\u{2500} Direction A",
                "    [Image: render.png [image/png] 1600x900]",
            ]
        );
        assert_eq!(fallback_count, fallback.len());
    }

    /// The un-boxed panel's label and body carry their shared spans with no box background.
    #[test]
    fn custom_panel_guttered_shape() {
        let row = CustomPanelRow {
            custom_type: "autonomous_status".to_string(),
            content: "[autonomous-status: on]".to_string(),
            image: None,
        };
        let rows = render_custom_panel(&row, &theme(), 40, MermaidMode::default());
        // No box background anywhere on the row.
        assert_eq!(
            rows[1][1],
            custom_message_label("autonomous_status", &theme())
        );
        assert!(rows[1].iter().all(|span| span.style.bg.is_none()));
        let body = rows[2]
            .iter()
            .find(|span| span.content.contains("[autonomous-status: on]"))
            .expect("the body text renders");
        assert_eq!(
            body.style.fg,
            theme().fg_style(ThemeColor::CustomMessageText).fg
        );
    }
}
