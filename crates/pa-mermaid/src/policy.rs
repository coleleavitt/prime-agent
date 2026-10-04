//! The fork's diagram policy (TS `components/mermaid.ts` `layoutMermaid`, `a358fd19e`):
//! how one Mermaid block is shown in the columns at hand.
//!
//! The renderer lays a diagram out at whatever width it needs and leaves fitting to the
//! caller, so a flowchart wider than the terminal is retried on the other axis (TD↔LR)
//! before falling back to its source, with a note giving the columns it needs. Warnings
//! are advisory: the art is still drawn and they are listed beside it. Notices are
//! omitted while streaming, where nearly every intermediate state warns.

use crate::render::{diagram_kind, render_cached, Art};

/// The axis a rotated flowchart is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// `LR`: the rotation of a vertical (`TB`/`TD`/`BT`) flowchart.
    LeftToRight,
    /// `TD`: the rotation of a horizontal (`LR`/`RL`) flowchart.
    TopToBottom,
}

/// A flowchart turned a quarter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotated {
    pub source: String,
    pub axis: Axis,
}

/// How loud a notice under a diagram is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
}

/// A line shown under a diagram (or its kept source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

/// How one Mermaid block is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// Draw this art (it fits), then the notices. `rotated`: the axis a too-wide flowchart
    /// was redrawn on to fit (`None`: drawn as written).
    Art {
        art: Art,
        notices: Vec<Notice>,
        rotated: Option<Axis>,
    },
    /// Keep the fenced source, then the notices.
    Source { notices: Vec<Notice> },
}

/// JS whitespace (`\s`).
fn is_js_space(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{0085}' && c.is_whitespace())
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// The JS `/^(\s*(?:flowchart|graph))(?:([ \t]+)(TB|TD|BT|LR|RL))?(?=[\s;]|%%|$)/i` over a
/// header line: `(keyword end, direction, match end)`.
fn match_header(line: &str) -> Option<(usize, Option<&str>, usize)> {
    let followed_ok = |at: usize| {
        let rest = &line[at..];
        rest.is_empty()
            || rest.starts_with(is_js_space)
            || rest.starts_with(';')
            || rest.starts_with("%%")
    };
    let lead = line.len() - line.trim_start_matches(is_js_space).len();
    let body = &line[lead..];
    let keyword = ["flowchart", "graph"].into_iter().find(|kw| {
        body.get(..kw.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(kw))
    })?;
    let keyword_end = lead + keyword.len();
    let gap = line[keyword_end..].len() - line[keyword_end..].trim_start_matches([' ', '\t']).len();
    if gap > 0 {
        let at = keyword_end + gap;
        if let Some(dir) = line.get(at..at + 2) {
            let known = ["TB", "TD", "BT", "LR", "RL"]
                .into_iter()
                .any(|d| d.eq_ignore_ascii_case(dir));
            if known && followed_ok(at + 2) {
                return Some((keyword_end, Some(dir), at + 2));
            }
        }
    }
    followed_ok(keyword_end).then_some((keyword_end, None, keyword_end))
}

/// The same flowchart turned a quarter: vertical layouts (TB/TD/BT) become LR, horizontal
/// ones (LR/RL) become TD. A diagram too wide for the terminal is usually a wide fan-out
/// or side-by-side subgraphs, which the other axis stacks. `None` for every other type.
#[must_use]
pub fn rotate_flowchart(source: &str) -> Option<Rotated> {
    let mut lines: Vec<&str> = source.split('\n').collect();
    let mut index = 0;
    while index < lines.len() && js_trim(lines[index]).is_empty() {
        index += 1;
    }
    if lines.get(index).map(|l| js_trim(l)) == Some("---") {
        index += 1;
        while index < lines.len() && js_trim(lines[index]) != "---" {
            index += 1;
        }
        index += 1;
    }
    while index < lines.len() && {
        let line = js_trim(lines[index]);
        line.is_empty() || line.starts_with("%%")
    } {
        index += 1;
    }
    let header = *lines.get(index).filter(|h| !h.is_empty())?;
    let (keyword_end, dir, match_end) = match_header(header)?;
    let current = dir.map_or_else(|| "TB".to_owned(), str::to_ascii_uppercase);
    let axis = if current == "LR" || current == "RL" {
        Axis::TopToBottom
    } else {
        Axis::LeftToRight
    };
    let token = match axis {
        Axis::LeftToRight => "LR",
        Axis::TopToBottom => "TD",
    };
    let rewritten = format!("{} {token}{}", &header[..keyword_end], &header[match_end..]);
    lines[index] = &rewritten;
    Some(Rotated {
        source: lines.join("\n"),
        axis,
    })
}

fn describe_warnings(warnings: &[String]) -> String {
    let more = if warnings.len() > 1 {
        format!(" (+{} more)", warnings.len() - 1)
    } else {
        String::new()
    };
    format!("Mermaid diagram incomplete: {}{more}", warnings[0])
}

/// Decide how one Mermaid block is shown in `available_width` columns (TS `layoutMermaid`).
#[must_use]
pub fn layout(source: &str, available_width: usize, streaming: bool) -> Layout {
    let mut notices = Vec::new();
    let art = render_cached(source);
    let art = art.as_ref().as_ref();
    let mut chosen: Option<Art> = art.filter(|a| a.width <= available_width).cloned();
    let mut rotated_on = None;
    let mut needed_width = art.map(|a| a.width);
    if chosen.is_none() {
        if let (Some(art), Some(rotated)) = (art, rotate_flowchart(source)) {
            let rotated_art = render_cached(&rotated.source);
            if let Some(rotated_art) = rotated_art.as_ref() {
                needed_width = Some(art.width.min(rotated_art.width));
                if rotated_art.width <= available_width {
                    chosen = Some(rotated_art.clone());
                    rotated_on = Some(rotated.axis);
                    if !streaming {
                        let axis = match rotated.axis {
                            Axis::LeftToRight => "left to right",
                            Axis::TopToBottom => "top to bottom",
                        };
                        notices.push(Notice {
                            level: NoticeLevel::Info,
                            text: format!(
                                "Mermaid diagram drawn {axis} to fit {available_width} columns"
                            ),
                        });
                    }
                }
            }
        }
    }

    if let Some(art) = chosen {
        if !streaming && !art.warnings.is_empty() {
            notices.push(Notice {
                level: NoticeLevel::Warning,
                text: describe_warnings(&art.warnings),
            });
        }
        return Layout::Art {
            art,
            notices,
            rotated: rotated_on,
        };
    }

    if !streaming {
        if let Some(needed) = needed_width {
            notices.push(Notice {
                level: NoticeLevel::Warning,
                text: format!(
                    "Mermaid diagram not drawn: needs {needed} columns, {available_width} available"
                ),
            });
        } else if diagram_kind(source).is_some() {
            notices.push(Notice {
                level: NoticeLevel::Warning,
                text: "Mermaid diagram not drawn: no statement could be parsed".to_owned(),
            });
        } else if let Some(header) = js_trim(source)
            .split(is_js_space)
            .next()
            .filter(|h| !h.is_empty())
        {
            notices.push(Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "Mermaid diagram not drawn: {header} is not supported in the terminal"
                ),
            });
        }
    }
    Layout::Source { notices }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
