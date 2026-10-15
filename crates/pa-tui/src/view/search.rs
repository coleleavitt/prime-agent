//! Transcript search (Claude Code's transcript `/` search, here `/search`):
//! the matching rows of the rendered transcript, the jump to one, and the
//! highlight of the match on screen. Rows are matched as rendered (the
//! current detail mode), case-insensitively.

use super::AgentView;
use crate::{Line, Span};

/// The highlighted match: its absolute transcript row and the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHighlight {
    pub(crate) row: usize,
    pub(crate) query: String,
}

fn row_text(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

/// Every char-index range of `query` in `text`, case-insensitively.
fn match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let haystack: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    // Lowercasing may change lengths (rare); ranges then index the folded
    // text, and a mismatch only shifts the highlight.
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    (0..=haystack.len() - needle.len())
        .filter(|&start| haystack[start..start + needle.len()] == needle[..])
        .map(|start| start..start + needle.len())
        .collect()
}

impl AgentView {
    /// The absolute rows of the rendered transcript that contain `query`,
    /// top to bottom. Resolves the window to absolute geometry (the rows the
    /// jump addresses).
    pub fn search_transcript(&mut self, query: &str) -> Vec<usize> {
        let query = query.trim();
        if query.is_empty() || self.layout_width == 0 {
            return Vec::new();
        }
        self.resolve_sparse_geometry();
        let layout = crate::image_component::with_fullscreen_image_fallback(|| {
            self.layout_pass(self.layout_width)
        });
        let rows = self.transcript_window(&layout, 0, usize::MAX);
        rows.iter()
            .enumerate()
            .filter(|(_, line)| !match_ranges(&row_text(line), query).is_empty())
            .map(|(row, _)| row)
            .collect()
    }

    /// Scroll so absolute transcript `row` sits a third of the way down the
    /// window, and highlight `query` on it.
    pub fn reveal_search_match(&mut self, row: usize, query: &str) {
        self.resolve_sparse_geometry();
        let top = row.saturating_sub(self.window_rows / 3);
        self.scroll_top = top.min(self.last_max_scroll);
        self.following = self.scroll_top >= self.last_max_scroll;
        self.search_highlight = Some(SearchHighlight {
            row,
            query: query.trim().to_string(),
        });
    }

    /// The highlighted match's row, while one is shown.
    pub fn search_match_row(&self) -> Option<usize> {
        self.search_highlight
            .as_ref()
            .map(|highlight| highlight.row)
    }

    pub fn clear_search_highlight(&mut self) {
        self.search_highlight = None;
    }

    /// Paint the highlighted match onto the window rows starting at absolute
    /// row `start` (absolute geometry only: a sparse window's rows carry no
    /// absolute index).
    pub(super) fn paint_search_highlight(&self, rows: &mut [Line], start: usize) {
        let Some(highlight) = &self.search_highlight else {
            return;
        };
        if self.sparse_window.is_some() {
            return;
        }
        let Some(line) = highlight
            .row
            .checked_sub(start)
            .and_then(|index| rows.get_mut(index))
        else {
            return;
        };
        let ranges = match_ranges(&row_text(line), &highlight.query);
        if ranges.is_empty() {
            return;
        }
        let style = self.theme.soft_selection_style();
        let mut painted: Line = Vec::with_capacity(line.len() + ranges.len() * 2);
        let mut index = 0;
        for span in line.drain(..) {
            let mut run = String::new();
            let mut run_marked = false;
            for c in span.content.chars() {
                let marked = ranges.iter().any(|range| range.contains(&index));
                if marked != run_marked && !run.is_empty() {
                    let run_style = if run_marked {
                        span.style.patch(style)
                    } else {
                        span.style
                    };
                    painted.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                run_marked = marked;
                run.push(c);
                index += 1;
            }
            if !run.is_empty() {
                let run_style = if run_marked {
                    span.style.patch(style)
                } else {
                    span.style
                };
                painted.push(Span::styled(run, run_style));
            }
        }
        *line = painted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_case_insensitive_and_overlapping_is_fine() {
        assert_eq!(
            match_ranges("Deploy the deploy", "deploy"),
            vec![0..6, 11..17]
        );
        assert_eq!(match_ranges("aaa", "aa"), vec![0..2, 1..3]);
        assert!(match_ranges("short", "").is_empty());
        assert!(match_ranges("ab", "abc").is_empty());
    }
}
