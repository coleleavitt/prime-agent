//! The feature status panel under the chat editor: the rows installed
//! features (a plugin's `set-status`, say) publish as `status.panel` in their
//! feature status. The daemon keeps each status on the session's roster
//! summary (`featureStatus.<feature> = {line, status}`) and pushes a
//! `feature_status` session event on every change; the chat view seeds from
//! the summary (a status published before this client attached) and patches
//! from the events. Rows are ordered by feature name and capped.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::theme::{Theme, ThemeColor};
use crate::width::truncate_to_width;
use crate::{Line, Span};

/// The most panel rows shown; the rest are summarized on the last row.
pub const MAX_PANEL_ROWS: usize = 6;

/// Each feature's panel rows, by feature name.
pub type FeaturePanels = BTreeMap<String, Vec<String>>;

/// A status's panel rows (`status.panel`, strings only; blank rows dropped).
#[must_use]
pub fn panel_of(status: &Value) -> Vec<String> {
    status
        .get("panel")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|row| row.replace(['\n', '\r', '\t'], " "))
        .filter(|row| !row.trim().is_empty())
        .collect()
}

/// The panels a roster summary carries (`featureStatus.<feature>.status`).
#[must_use]
pub fn panels_from_summary(summary: &Value) -> FeaturePanels {
    summary
        .get("featureStatus")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(feature, entry)| {
            let rows = panel_of(entry.get("status").unwrap_or(&Value::Null));
            (!rows.is_empty()).then(|| (feature.clone(), rows))
        })
        .collect()
}

/// Apply one `feature_status` event: its panel replaces the feature's, and
/// an empty one removes it.
pub fn apply_status(panels: &mut FeaturePanels, feature: &str, status: &Value) {
    let rows = panel_of(status);
    if rows.is_empty() {
        panels.remove(feature);
    } else {
        panels.insert(feature.to_string(), rows);
    }
}

/// The rows to show, in feature-name order, at most [`MAX_PANEL_ROWS`]: a
/// longer panel set ends with a `+N more` row.
#[must_use]
pub fn rows(panels: &FeaturePanels) -> Vec<String> {
    let all: Vec<&String> = panels.values().flatten().collect();
    if all.len() <= MAX_PANEL_ROWS {
        return all.into_iter().cloned().collect();
    }
    let mut shown: Vec<String> = all[..MAX_PANEL_ROWS - 1]
        .iter()
        .map(|row| (*row).clone())
        .collect();
    shown.push(format!("+{} more", all.len() - (MAX_PANEL_ROWS - 1)));
    shown
}

/// The panel's lines: one muted row each, truncated to the width.
#[must_use]
pub fn render(rows: &[String], theme: &Theme, width: usize) -> Vec<Line> {
    let muted = theme.fg_style(ThemeColor::Muted);
    rows.iter()
        .map(|row| vec![Span::styled(truncate_to_width(row, width, "…"), muted)])
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn panels_seed_from_a_summary_and_follow_status_events() {
        let mut panels = panels_from_summary(&json!({
            "featureStatus": {
                "plugin:zeta": { "line": "z", "status": { "panel": ["zeta row"] } },
                "plugin:alpha": { "line": "a", "status": { "panel": ["alpha one", "", "alpha\ttwo"] } },
                "anthropic-auth": { "line": "quota", "status": { "used": 3 } },
            }
        }));
        assert_eq!(rows(&panels), vec!["alpha one", "alpha two", "zeta row"]);
        apply_status(&mut panels, "plugin:zeta", &json!({ "panel": [] }));
        apply_status(&mut panels, "plugin:mid", &json!({ "panel": ["mid row"] }));
        assert_eq!(rows(&panels), vec!["alpha one", "alpha two", "mid row"]);
    }

    #[test]
    fn a_long_panel_set_is_capped_with_a_summary_row() {
        let panels: FeaturePanels = [(
            "plugin:p".to_string(),
            (1..=9).map(|n| format!("row {n}")).collect(),
        )]
        .into_iter()
        .collect();
        assert_eq!(
            rows(&panels),
            vec!["row 1", "row 2", "row 3", "row 4", "row 5", "+4 more"]
        );
    }

    #[test]
    fn rows_render_muted_and_truncated() {
        let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
        let lines = render(&["a long panel row".to_string()], &theme, 8);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].iter().map(|span| span.content.as_str()).collect();
        assert_eq!(text, "a long …");
        assert_eq!(lines[0][0].style, theme.fg_style(ThemeColor::Muted));
    }
}
