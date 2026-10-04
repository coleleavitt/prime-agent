//! `pie`: proportions as a labelled bar list. Ported from lovely-mermaid 0.3.3
//! `diagrams/pie.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! A terminal has no circle worth drawing; bars carry the same information in less space.
//! Lenient: an unreadable statement is dropped and recorded.

use super::super::canvas::{draw_text, Canvas};
use super::super::graph::MAX_NODES;
use super::super::js_number;
use super::super::js_text;
use super::super::labels::{clean_label, fit_label};
use super::super::layout::width_of;
use super::super::statements::{header_kind, quote_mask, statements_of};
use super::super::Role;
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["pie"];

/// Columns of the full-scale bar; eighth blocks refine below one cell.
const BAR_W: i64 = 20;
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

struct Slice {
    label: String,
    value: f64,
}

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let parsed = parse_pie(src)?;

    let label_w = parsed
        .slices
        .iter()
        .map(|s| width_of(&s.label))
        .max()
        .unwrap_or(0);
    let total = parsed.slices.iter().fold(0.0, |sum, s| sum + s.value);
    let rows: Vec<(&Slice, f64, String)> = parsed
        .slices
        .iter()
        .map(|s| {
            let share = if total == 0.0 { 0.0 } else { s.value / total };
            let pct = format!(
                "{:>4}",
                format!("{}%", js_number::to_string(js_number::round(share * 100.0)))
            );
            let data = if parsed.show_data {
                format!("  ({})", js_number::to_string(s.value))
            } else {
                String::new()
            };
            (s, share, pct + &data)
        })
        .collect();

    let bar_x = label_w + 2;
    let suffix_x = bar_x + BAR_W + 1;
    let width = suffix_x
        + rows
            .iter()
            .map(|(_, _, suffix)| width_of(suffix))
            .max()
            .unwrap_or(0);
    let top = i64::from(parsed.title.is_some());
    let mut canvas = Canvas::new(width, top + rows.len() as i64);

    if let Some(title) = &parsed.title {
        let x = ((width - width_of(title)).div_euclid(2)).max(0);
        draw_text(&mut canvas, title, x, 0, Role::Title);
    }
    for (i, (slice, share, suffix)) in rows.iter().enumerate() {
        let y = top + i as i64;
        draw_text(&mut canvas, &slice.label, 0, y, Role::Text);
        let eighths = js_number::round(share * BAR_W as f64 * 8.0) as i64;
        let mut bar =
            "█".repeat(eighths.div_euclid(8) as usize) + EIGHTHS[eighths.rem_euclid(8) as usize];
        // A nonzero slice always shows at least a sliver.
        if bar.is_empty() && slice.value > 0.0 {
            "▏".clone_into(&mut bar);
        }
        draw_text(&mut canvas, &bar, bar_x, y, Role::Edge);
        // The unfilled remainder is a track, so every bar shows its full scale.
        let bar_w = width_of(&bar);
        let track = "░".repeat((BAR_W - bar_w).max(0) as usize);
        draw_text(&mut canvas, &track, bar_x + bar_w, y, Role::Border);
        draw_text(&mut canvas, suffix, suffix_x, y, Role::EdgeLabel);
    }
    Some(Drawn {
        canvas,
        warnings: parsed.warnings,
    })
}

struct Pie {
    title: Option<String>,
    slices: Vec<Slice>,
    show_data: bool,
    warnings: Vec<String>,
}

/// JS `s.slice(s.toLowerCase().indexOf(word) + word.length).trim()`, `None` when empty.
fn after_keyword(st: &str, word: &str) -> Option<String> {
    let at = st.to_lowercase().find(word).unwrap_or(0) + word.len();
    let rest = js_text::trim(st.get(at..).unwrap_or(""));
    (!rest.is_empty()).then(|| rest.to_owned())
}

fn parse_pie(src: &str) -> Option<Pie> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "pie" {
        return None;
    }

    // The header line may carry `showData` and an inline `title …`.
    let head = js_text::words(&statements[0]);
    let show_data = head.iter().any(|w| w.to_lowercase() == "showdata");
    let mut title = head
        .iter()
        .position(|w| w.to_lowercase() == "title")
        .and_then(|at| {
            let joined = head[at + 1..].join(" ");
            (!joined.is_empty()).then_some(joined)
        });

    let mut slices = Vec::new();
    let mut warnings = Vec::new();
    let mut truncated = false;
    for st in &statements[1..] {
        if js_text::words(st)
            .first()
            .map(|w| w.to_lowercase())
            .as_deref()
            == Some("title")
        {
            title = after_keyword(st, "title");
            continue;
        }
        let Some(slice) = parse_slice(st) else {
            warnings.push(format!("dropped, unreadable statement: \"{st}\""));
            continue;
        };
        if slices.len() >= MAX_NODES {
            truncated = true;
            break;
        }
        slices.push(slice);
    }
    if truncated {
        warnings.push(format!(
            "diagram truncated: slice cap ({MAX_NODES}) reached"
        ));
    }

    (!slices.is_empty()).then_some(Pie {
        title,
        slices,
        show_data,
        warnings,
    })
}

/// `"Label" : 42.5` — the label may be unquoted as long as it has no colon.
fn parse_slice(st: &str) -> Option<Slice> {
    let chars: Vec<char> = st.chars().collect();
    let quoted = quote_mask(&chars);
    let colon = (0..chars.len()).find(|&i| chars[i] == ':' && !quoted[i])?;
    let label_raw: String = chars[..colon].iter().collect();
    let label = fit_label(&clean_label(&label_raw), 24);
    let value_raw: String = chars[colon + 1..].iter().collect();
    let value = js_number::parse(js_text::trim(&value_raw))?;
    if label.is_empty() || !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(Slice { label, value })
}
