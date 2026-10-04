//! Feature status sub-lines (the roster summary's `featureStatus.<feature>.line`, TS
//! `renderRavoRow`/`renderDreamRow`): one dim line per feature under its agent row, indented one
//! level past the row, hard-clipped to the width. The list's variable-height rows keep the
//! viewport, the click surface, and the hover band in agreement with what is painted.

use super::*;

/// A top-level session summary carrying the given feature status lines (`None` clears one).
fn summary_with_status(id: &str, statuses: &[(&str, Option<&str>)]) -> serde_json::Value {
    let mut summary = parent_summary(id);
    if !statuses.is_empty() {
        let map: serde_json::Map<String, serde_json::Value> = statuses
            .iter()
            .map(|(feature, line)| {
                (
                    (*feature).to_string(),
                    serde_json::json!({ "line": line, "status": { "phase": "running" } }),
                )
            })
            .collect();
        summary["featureStatus"] = serde_json::Value::Object(map);
    }
    summary
}

/// A view over sessions `ids` (all idle), where `with_status` carries the given statuses.
fn status_mode(
    ids: &[&str],
    with_status: &str,
    statuses: &[(&str, Option<&str>)],
) -> AgentsViewMode {
    fresh_mode(
        ids.iter()
            .map(|id| {
                let summary = if *id == with_status {
                    summary_with_status(id, statuses)
                } else {
                    parent_summary(id)
                };
                roster_entry(id, "idle", &summary)
            })
            .collect(),
    )
}

fn flat_frame(lines: &[Line]) -> Vec<String> {
    lines.iter().map(flat).collect()
}

fn row_of(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} renders: {lines:#?}"))
}

/// One sub-line's exact painted text: one indent level past a top-level row, padded.
fn sub_line(text: &str, width: usize) -> String {
    let line = format!("  {text}");
    format!("{line}{}", " ".repeat(width - str_width(&line)))
}

/// The status-less frame with `inserted` spliced in under the row holding `needle`: the
/// sub-lines push the rest of the list down by their count, which the blank fill above the
/// hint line absorbs.
fn spliced(baseline: &[String], needle: &str, inserted: &[String]) -> Vec<String> {
    let mut frame = baseline.to_vec();
    let row = row_of(&frame, needle);
    for (offset, line) in inserted.iter().enumerate() {
        frame.insert(row + 1 + offset, line.clone());
    }
    for _ in inserted {
        let blank = frame.len() - 2;
        assert_eq!(
            frame[blank], "",
            "the fill above the hint line absorbs the sub-lines"
        );
        frame.remove(blank);
    }
    frame
}

#[test]
fn one_status_line_renders_dim_under_its_row() {
    let mut baseline = status_mode(&["a", "b"], "a", &[]);
    let mut mode = status_mode(&["a", "b"], "a", &[("stub", Some("stub running r2/1"))]);
    let (lines, _) = baseline.render_frame(120, 30);
    let expected = spliced(
        &flat_frame(&lines),
        "a name",
        &[sub_line("stub running r2/1", 120)],
    );
    let (lines, _) = mode.render_frame(120, 30);
    assert_eq!(flat_frame(&lines), expected);
    let row = row_of(&expected, "stub running r2/1");
    assert_eq!(
        lines[row][0],
        mode.theme
            .fg(ThemeColor::Dim, "  stub running r2/1".to_string()),
        "the sub-line is dim"
    );
}

#[test]
fn two_status_lines_render_by_feature_name() {
    let mut baseline = status_mode(&["a", "b"], "b", &[]);
    let mut mode = status_mode(
        &["a", "b"],
        "b",
        &[
            ("zeta", Some("zeta last")),
            ("alpha", Some("alpha first")),
            ("cleared", None),
        ],
    );
    let (lines, _) = baseline.render_frame(120, 30);
    let expected = spliced(
        &flat_frame(&lines),
        "b name",
        &[sub_line("alpha first", 120), sub_line("zeta last", 120)],
    );
    let (lines, _) = mode.render_frame(120, 30);
    assert_eq!(flat_frame(&lines), expected);
}

/// Only cleared statuses (the native case's shape): the frame is byte-identical to a
/// status-less one.
#[test]
fn cleared_statuses_render_the_native_frame() {
    let mut baseline = status_mode(&["a", "b"], "a", &[]);
    let mut mode = status_mode(&["a", "b"], "a", &[("stub", None), ("other", Some("  "))]);
    assert_eq!(mode.render_frame(120, 30), baseline.render_frame(120, 30));
    assert_eq!(mode.click_rows, baseline.click_rows);
}

/// A click on a status sub-line selects and opens its session (red-first: the sub-line was not
/// on the click surface).
#[test]
fn a_click_on_a_status_sub_line_opens_its_session() {
    let _guard = match crate::mouse_tracking::STATE_TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    crate::mouse_tracking::enable(&mut std::io::stdout()).expect("enable");
    let mut mode = status_mode(&["a", "b"], "b", &[("stub", Some("stub running"))]);
    let b = mode
        .rows
        .iter()
        .position(|row| row.summary["sessionId"] == "b")
        .expect("b's row");
    assert_ne!(mode.selected, b, "the click must move the selection");
    let (lines, _) = mode.render_frame(120, 30);
    let row = row_of(&flat_frame(&lines), "stub running");
    mode.handle_mouse(&mouse_report(row, true, false));
    mode.handle_mouse(&mouse_report(row, false, false));
    assert_eq!(
        mode.selected, b,
        "the click selected the sub-line's session"
    );
    assert_eq!(
        mode.opened.as_ref().map(|opened| opened.selection.clone()),
        Some(SessionSelection::Attach("b-live".to_string())),
        "the click opened the sub-line's session"
    );
    crate::mouse_tracking::disable(&mut std::io::stdout()).expect("disable");
}

/// A motion over a sub-line hovers its session: the band rides the session row, never the
/// sub-line.
#[test]
fn a_hovered_status_sub_line_bands_its_session_row() {
    let _guard = match crate::mouse_tracking::STATE_TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    crate::mouse_tracking::enable(&mut std::io::stdout()).expect("enable");
    let mut mode = status_mode(&["a", "b"], "b", &[("stub", Some("stub running"))]);
    let (lines, _) = mode.render_frame(120, 30);
    let text = flat_frame(&lines);
    let (row, sub) = (row_of(&text, "b name"), row_of(&text, "stub running"));
    mode.handle_mouse(&crate::mouse::MouseEvent {
        button: crate::mouse::BUTTON_NONE,
        x: 3,
        y: (sub + 1) as u16,
        press: true,
        motion: true,
        shift: false,
        alt: false,
        ctrl: false,
    });
    assert_eq!(mode.hover_row, Some(sub));
    let (lines, _) = mode.render_frame(120, 30);
    let band = mode.theme.hover_row_style().bg;
    assert!(
        lines[row].iter().all(|span| span.style.bg == band),
        "the session row carries the band: {:?}",
        lines[row]
    );
    assert!(
        lines[sub].iter().all(|span| span.style.bg != band),
        "the sub-line stays unbanded: {:?}",
        lines[sub]
    );
    crate::mouse_tracking::disable(&mut std::io::stdout()).expect("disable");
}

/// Scrolling a list taller than the pane: every selection keeps its row AND its sub-lines in
/// view, and every click row lands on the session's own painted row or sub-line.
#[test]
fn scrolling_keeps_the_selected_block_in_view_and_the_click_rows_on_their_lines() {
    let ids: Vec<String> = (0..12).map(|n| format!("s{n:02}")).collect();
    let roster = ids
        .iter()
        .map(|id| {
            let summary = summary_with_status(
                id,
                &[
                    ("alpha", Some(&format!("{id} alpha"))),
                    ("beta", Some(&format!("{id} beta"))),
                ],
            );
            roster_entry(id, "idle", &summary)
        })
        .collect();
    let mut mode = fresh_mode(roster);
    for step in 0..ids.len() {
        let (lines, _) = mode.render_frame(80, 24);
        let text = flat_frame(&lines);
        let id = mode.rows[mode.selected].summary["sessionId"]
            .as_str()
            .expect("id")
            .to_string();
        let row = row_of(&text, &format!("{id} name"));
        assert_eq!(
            text[row + 1..row + 3],
            [
                sub_line(&format!("{id} alpha"), 80),
                sub_line(&format!("{id} beta"), 80)
            ],
            "step {step}: the selected block renders whole: {text:#?}"
        );
        for (click_row, index) in &mode.click_rows {
            let session = mode.rows[*index].summary["sessionId"].as_str().expect("id");
            let painted = &text[*click_row];
            assert!(
                painted.contains(&format!("{session} name"))
                    || painted.trim_end() == format!("  {session} alpha")
                    || painted.trim_end() == format!("  {session} beta"),
                "step {step}: click row {click_row} ({session}) paints {painted:?}"
            );
        }
        let painted_sub_lines = text
            .iter()
            .filter(|line| {
                line.starts_with("  s") && (line.contains(" alpha") || line.contains(" beta"))
            })
            .count();
        assert!(painted_sub_lines >= 2, "step {step}: sub-lines paint");
        assert_eq!(
            mode.click_rows.len(),
            text.iter().filter(|line| line.contains(" name")).count() + painted_sub_lines,
            "step {step}: every painted row and sub-line is a click row"
        );
        mode.handle_key("down");
    }
}

/// A narrow pane hard-clips the sub-line to the width (no ellipsis, TS `truncateToWidth(.., "")`).
#[test]
fn a_narrow_pane_clips_the_sub_line_to_the_width() {
    let mut mode = status_mode(
        &["a"],
        "a",
        &[("stub", Some("stub evaluate r2/1 · reject_deep 41"))],
    );
    let (lines, _) = mode.render_frame(16, 30);
    let text = flat_frame(&lines);
    let row = row_of(&text, "  stub evaluate");
    assert_eq!(text[row], "  stub evaluate ");
    assert_eq!(str_width(&text[row]), 16);
}

/// The passivation path: the daemon strips `featureStatus` from a passivated summary, and the
/// roster update that carries it clears the sub-line — the frame is the status-less one again.
#[test]
fn a_passivated_summary_clears_the_sub_line() {
    let mut baseline = status_mode(&["a", "b"], "a", &[]);
    let mut mode = status_mode(&["a", "b"], "a", &[("stub", Some("stub running"))]);
    let (lines, _) = mode.render_frame(120, 30);
    assert!(flat_frame(&lines)
        .iter()
        .any(|line| line.contains("stub running")));
    let mut passivated = parent_summary("a");
    let object = passivated.as_object_mut().expect("object");
    object.remove("activeSessionId");
    mode.apply_roster_update(
        vec![roster_entry("a", "idle", &passivated)],
        Vec::new(),
        false,
    );
    baseline.apply_roster_update(
        vec![roster_entry("a", "idle", &passivated)],
        Vec::new(),
        false,
    );
    assert_eq!(mode.render_frame(120, 30), baseline.render_frame(120, 30));
}
