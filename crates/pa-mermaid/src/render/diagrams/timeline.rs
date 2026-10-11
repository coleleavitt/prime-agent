//! `timeline`: periods and their events as a vertical list — one row per event, the
//! period named on its first row. Ported from lovely-mermaid 0.3.3 `diagrams/timeline.ts`
//! (Apache-2.0; see `LICENSE-lovely-mermaid`).

use super::super::canvas::{Canvas, draw_text};
use super::super::graph::MAX_EDGES;
use super::super::labels::{MAX_LABEL, decode_html_entities, fit_label};
use super::super::layout::width_of;
use super::super::statements::{header_kind, statements_of};
use super::super::{Role, js_text};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["timeline"];

/// One output row: a period cell (blank on continuations) and an event; section headers
/// take a row of their own.
struct Row {
    period: String,
    event: String,
    section: bool,
}

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let (title, rows, warnings) = parse_timeline(src)?;

    let period_w = rows
        .iter()
        .map(|r| if r.section { 0 } else { width_of(&r.period) })
        .max()
        .unwrap_or(0);
    let top = i64::from(title.is_some());
    let width = rows
        .iter()
        .map(|r| {
            if r.section {
                width_of(&r.event)
            } else {
                period_w + 3 + width_of(&r.event)
            }
        })
        .fold(title.as_deref().map_or(0, |t| width_of(t) + 6), i64::max);
    let mut canvas = Canvas::new(width, top + rows.len() as i64);

    if let Some(title) = &title {
        let t = format!(" {title} ");
        let x = (width - width_of(&t) - 4).div_euclid(2).max(0);
        draw_text(&mut canvas, "──", x, 0, Role::Edge);
        draw_text(&mut canvas, &t, x + 2, 0, Role::Title);
        draw_text(&mut canvas, "──", x + 2 + width_of(&t), 0, Role::Edge);
    }
    for (i, row) in rows.iter().enumerate() {
        let y = top + i as i64;
        if row.section {
            draw_text(&mut canvas, &row.event, 0, y, Role::Title);
            continue;
        }
        draw_text(&mut canvas, &row.period, 0, y, Role::Text);
        if row.event.is_empty() {
            continue;
        }
        draw_text(&mut canvas, "─", period_w + 1, y, Role::Edge);
        draw_text(&mut canvas, &row.event, period_w + 3, y, Role::EdgeLabel);
    }
    Some(Drawn { canvas, warnings })
}

fn clean(s: &str) -> String {
    fit_label(&decode_html_entities(js_text::trim(s)), MAX_LABEL)
}

/// The text after the first occurrence of `word` (case-insensitive), trimmed.
fn after_keyword<'a>(st: &'a str, word: &str) -> &'a str {
    let at = st.to_lowercase().find(word).unwrap_or(0) + word.len();
    js_text::trim(st.get(at..).unwrap_or(""))
}

type Parsed = (Option<String>, Vec<Row>, Vec<String>);

fn parse_timeline(src: &str) -> Option<Parsed> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "timeline" {
        return None;
    }

    let mut title = None;
    let mut rows: Vec<Row> = Vec::new();
    let mut warnings = Vec::new();
    let mut truncated = false;
    let mut last_period = false;

    for st in &statements[1..] {
        if rows.len() >= MAX_EDGES {
            truncated = true;
            break;
        }
        let first = js_text::words(st).first().map(|w| w.to_lowercase());
        match first.as_deref() {
            Some("title") => {
                let rest = after_keyword(st, "title");
                title = (!rest.is_empty()).then(|| rest.to_owned());
                continue;
            }
            Some("section") => {
                rows.push(Row {
                    period: String::new(),
                    event: clean(after_keyword(st, "section")),
                    section: true,
                });
                last_period = false;
                continue;
            }
            _ => {}
        }
        // `period : event : event`; a statement of only `: event`s continues the previous
        // period (the `;`-split form of multi-line events).
        let mut parts = st.split(':').map(clean);
        let period = parts.next().unwrap_or_default();
        let events: Vec<String> = parts.collect();
        if period.is_empty() && !events.is_empty() && last_period {
            for event in events {
                rows.push(Row {
                    period: String::new(),
                    event,
                    section: false,
                });
            }
            continue;
        }
        // A bare period renders event-less.
        if !period.is_empty() && events.is_empty() {
            rows.push(Row {
                period,
                event: String::new(),
                section: false,
            });
            last_period = true;
            continue;
        }
        if period.is_empty() || events.iter().any(String::is_empty) {
            warnings.push(format!("dropped, unreadable statement: \"{st}\""));
            continue;
        }
        for (i, event) in events.into_iter().enumerate() {
            rows.push(Row {
                period: if i == 0 {
                    period.clone()
                } else {
                    String::new()
                },
                event,
                section: false,
            });
        }
        last_period = true;
    }
    if truncated {
        warnings.push(format!(
            "diagram truncated: event cap ({MAX_EDGES}) reached"
        ));
    }

    (!rows.is_empty()).then_some((title, rows, warnings))
}
