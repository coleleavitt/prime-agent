//! A dependency-free ASCII plot for `prime-agent learning` (TS
//! `learning-chart.ts`): series over one day axis on a fixed grid, so the
//! output diffs cleanly and works over a pipe. With more days than columns
//! the days are bucketed and averaged, so the axis never wraps.

use crate::js::{js_round, pad_start, to_fixed};

/// One plotted series.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub label: String,
    /// The character drawn for this series; two series on one cell draw `*`.
    pub mark: char,
    pub points: Vec<Option<f64>>,
}

/// Chart layout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChartOptions {
    pub height: Option<usize>,
    pub width: Option<usize>,
    /// One label per point; the first and the last are printed.
    pub x_labels: Vec<String>,
    /// A point to mark on the axis (the pivot).
    pub marker_index: Option<usize>,
    pub value_label: Option<String>,
}

const DEFAULT_HEIGHT: usize = 9;
const DEFAULT_WIDTH: usize = 56;
const GUTTER: usize = 7;
const BOTH_MARK: char = '*';
const EMPTY_CHART: &str = "(no data to chart)";

fn mean(values: &[f64]) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a handful of days")]
    let len = values.len() as f64;
    values.iter().fold(0.0, |sum, value| sum + value) / len
}

fn bucket(points: &[Option<f64>], columns: usize) -> Vec<Option<f64>> {
    if points.len() <= columns {
        return points.to_vec();
    }
    (0..columns)
        .map(|column| {
            let start = column * points.len() / columns;
            let end = (start + 1).max((column + 1) * points.len() / columns);
            let values: Vec<f64> = points[start..end.min(points.len())]
                .iter()
                .flatten()
                .copied()
                .collect();
            (!values.is_empty()).then(|| mean(&values))
        })
        .collect()
}

fn bucket_index(index: usize, length: usize, columns: usize) -> usize {
    if length <= columns {
        return index;
    }
    (columns - 1).min(index * columns / length)
}

fn format_axis_value(value: f64) -> String {
    let text = if value.abs() >= 100.0 {
        to_fixed(value, 0)
    } else {
        to_fixed(value, 2)
    };
    pad_start(&text, GUTTER - 2)
}

/// The chart's lines, trailing spaces trimmed.
#[must_use]
pub fn render_ascii_chart(series: &[ChartSeries], options: &ChartOptions) -> Vec<String> {
    let height = options.height.unwrap_or(DEFAULT_HEIGHT).max(3);
    let length = series
        .iter()
        .map(|entry| entry.points.len())
        .max()
        .unwrap_or(0);
    if length == 0 || series.is_empty() {
        return vec![EMPTY_CHART.to_string()];
    }
    let columns = options.width.unwrap_or(DEFAULT_WIDTH).min(length).max(8);
    let bucketed: Vec<(char, &str, Vec<Option<f64>>)> = series
        .iter()
        .map(|entry| {
            (
                entry.mark,
                entry.label.as_str(),
                bucket(&entry.points, columns),
            )
        })
        .collect();
    let values: Vec<f64> = bucketed
        .iter()
        .flat_map(|(_, _, points)| points.iter().flatten().copied())
        .collect();
    if values.is_empty() {
        return vec![EMPTY_CHART.to_string()];
    }
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = values.iter().copied().fold(0.0_f64, f64::min);
    let span = if max - min == 0.0 { 1.0 } else { max - min };
    let mut grid = vec![vec![' '; columns]; height];
    for (mark, _, points) in &bucketed {
        for (column, value) in points.iter().enumerate() {
            let Some(value) = value else {
                continue;
            };
            #[expect(clippy::cast_precision_loss, reason = "a chart height")]
            let scaled = js_round(((max - value) / span) * (height - 1) as f64);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to the grid rows"
            )]
            let row = (scaled.max(0.0) as usize).min(height - 1);
            let cell = grid[row][column];
            grid[row][column] = if cell == ' ' || cell == *mark {
                *mark
            } else {
                BOTH_MARK
            };
        }
    }

    let gutter = " ".repeat(GUTTER);
    let mut lines: Vec<String> = Vec::new();
    if let Some(label) = &options.value_label {
        lines.push(format!("{gutter}{label}"));
    }
    for (row, cells) in grid.iter().enumerate() {
        let label = if row == 0 {
            format_axis_value(max)
        } else if row == height - 1 {
            format_axis_value(min)
        } else {
            " ".repeat(GUTTER - 2)
        };
        let cells: String = cells.iter().collect();
        lines.push(format!("{label} |{cells}"));
    }
    lines.push(format!(
        "{} +{}",
        " ".repeat(GUTTER - 2),
        "-".repeat(columns)
    ));
    let mut axis = vec![' '; columns];
    if let Some(marker) = options.marker_index.filter(|marker| *marker < length) {
        axis[bucket_index(marker, length, columns)] = '^';
    }
    if axis.iter().any(|cell| *cell != ' ') {
        lines.push(format!("{gutter}{}", axis.iter().collect::<String>()));
    }
    if let (Some(first), Some(last)) = (options.x_labels.first(), options.x_labels.last()) {
        if !first.is_empty() && !last.is_empty() {
            let gap = columns
                .saturating_sub(crate::js::js_len(first) + crate::js::js_len(last))
                .max(1);
            lines.push(format!("{gutter}{first}{}{last}", " ".repeat(gap)));
        }
    }
    let legend: Vec<String> = bucketed
        .iter()
        .map(|(mark, label, _)| format!("{mark} {label}"))
        .collect();
    lines.push(format!("{gutter}{}   {BOTH_MARK} both", legend.join("   ")));
    lines
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}
