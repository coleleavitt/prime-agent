//! `/search <text>`: find text in the transcript and jump to the newest
//! match; `app.search.older` (F3) and `app.search.newer` (Shift+F3) walk the
//! matches; Escape clears the highlight. Every step searches the transcript
//! as it is rendered now, so new rows and detail changes stay in step.

use super::{AgentView, SessionUi};

/// Which way a search step moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchStep {
    Older,
    Newer,
}

/// The match `step` lands on from `current` (`None`: the newest match), in
/// top-to-bottom `matches`; `None` past either end.
fn next_match(matches: &[usize], current: Option<usize>, step: SearchStep) -> Option<usize> {
    let Some(current) = current else {
        return matches.last().copied();
    };
    match step {
        SearchStep::Older => matches.iter().rev().find(|row| **row < current).copied(),
        SearchStep::Newer => matches.iter().find(|row| **row > current).copied(),
    }
}

impl SessionUi {
    /// `/search <text>`: jump to the newest match.
    pub(super) fn start_search(&mut self, query: &str, view: &mut AgentView) {
        let query = query.trim();
        if query.is_empty() {
            self.note(
                "Usage: /search <text> (F3 older match, Shift+F3 newer, Esc clears)",
                view,
            );
            return;
        }
        self.search_query = Some(query.to_string());
        view.clear_search_highlight();
        self.step_search(None, view);
    }

    /// Move to the older or newer match (`None`: the newest).
    pub(super) fn step_search(&mut self, step: Option<SearchStep>, view: &mut AgentView) {
        let Some(query) = self.search_query.clone() else {
            self.note("No search: /search <text> starts one", view);
            return;
        };
        let matches = view.search_transcript(&query);
        let current = view.search_match_row();
        let target = match step {
            None => next_match(&matches, None, SearchStep::Older),
            Some(step) => next_match(&matches, current, step),
        };
        match target {
            Some(row) => {
                view.reveal_search_match(row, &query);
                let position = matches.iter().position(|m| *m == row).unwrap_or(0) + 1;
                self.toast(
                    &format!("Match {position} of {} for \"{query}\"", matches.len()),
                    view,
                );
            }
            None if matches.is_empty() => {
                self.search_query = None;
                view.clear_search_highlight();
                self.note(&format!("No matches for \"{query}\""), view);
            }
            None => self.toast(
                match step {
                    Some(SearchStep::Newer) => "No newer match",
                    _ => "No older match",
                },
                view,
            ),
        }
        self.dirty = true;
    }

    /// Escape with a search shown: the highlight clears (the press is
    /// spent). `false` when no search was shown.
    pub(super) fn clear_search(&mut self, view: &mut AgentView) -> bool {
        if self.search_query.take().is_none() && view.search_match_row().is_none() {
            return false;
        }
        view.clear_search_highlight();
        self.dirty = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_walk_the_matches_and_stop_at_the_ends() {
        let matches = [3, 10, 42];
        assert_eq!(next_match(&matches, None, SearchStep::Older), Some(42));
        assert_eq!(next_match(&matches, Some(42), SearchStep::Older), Some(10));
        assert_eq!(next_match(&matches, Some(3), SearchStep::Older), None);
        assert_eq!(next_match(&matches, Some(10), SearchStep::Newer), Some(42));
        assert_eq!(next_match(&matches, Some(42), SearchStep::Newer), None);
        // A current row that is no longer a match still steps from its place.
        assert_eq!(next_match(&matches, Some(20), SearchStep::Older), Some(10));
        assert_eq!(next_match(&[], None, SearchStep::Older), None);
    }
}
