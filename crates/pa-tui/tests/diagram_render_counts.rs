//! The run's diagram render counts (`pa_tui::diagram::take_render_counts`) under the
//! built-in renderer: each settled diagram counts once however often it repaints, a
//! streaming paint never counts, the off mode counts nothing, and a fence kept as source
//! counts as such. (The counts are process-wide, so this binary's tests take turns.)

use std::sync::{Mutex, MutexGuard};

use pa_tui::diagram::{RenderCounts, set_render_counting, take_render_counts};
use pa_tui::markdown::{MarkdownStyle, MermaidMode, render_markdown};

const TWO_DIAGRAMS: &str = "Flow:\n\n```mermaid\nflowchart TD\n  A --> B\n```\n\nThen:\n\n\
```mermaid\nsequenceDiagram\n  A->>B: hi\n```\n";

fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    let guard = TURN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_render_counting(true);
    let _ = take_render_counts();
    guard
}

fn paint(text: &str, width: usize, mode: MermaidMode, streaming: bool) {
    let style = MarkdownStyle::default().with_mermaid(mode, streaming);
    let _ = render_markdown(text, width, &style);
}

#[test]
fn each_settled_diagram_counts_once_despite_repaints() {
    let _turn = turn();
    for _ in 0..3 {
        paint(TWO_DIAGRAMS, 80, MermaidMode::Streaming, true);
    }
    for width in [80, 80, 100, 80] {
        paint(TWO_DIAGRAMS, width, MermaidMode::Streaming, false);
    }
    assert_eq!(
        take_render_counts(),
        RenderCounts {
            drawn: 2,
            kept_source: 0,
            adapted: 0,
        }
    );
    // The take starts the next run: the same diagrams count again there.
    paint(TWO_DIAGRAMS, 80, MermaidMode::Final, false);
    assert_eq!(take_render_counts().drawn, 2);
}

#[test]
fn the_off_mode_counts_nothing() {
    let _turn = turn();
    paint(TWO_DIAGRAMS, 80, MermaidMode::Off, false);
    paint(TWO_DIAGRAMS, 80, MermaidMode::Final, true);
    assert_eq!(take_render_counts(), RenderCounts::default());
}

#[test]
fn fences_kept_as_source_count_as_kept() {
    let _turn = turn();
    // Unsupported type, then a diagram wider than the transcript.
    paint(
        "```mermaid\npie\n  \"a\" : 1\n```\n",
        80,
        MermaidMode::Streaming,
        false,
    );
    let wide = "```mermaid\nflowchart LR\n  Alpha_node_one --> Bravo_node_two --> Charlie_node_three --> Delta_node_four\n```\n";
    paint(wide, 30, MermaidMode::Streaming, false);
    paint(wide, 30, MermaidMode::Streaming, false);
    assert_eq!(
        take_render_counts(),
        RenderCounts {
            drawn: 0,
            kept_source: 2,
            adapted: 0,
        }
    );
}

#[test]
fn nothing_counts_while_counting_is_off_and_turning_it_off_drops_the_counts() {
    let _turn = turn();
    paint(TWO_DIAGRAMS, 80, MermaidMode::Streaming, false);
    set_render_counting(false);
    assert_eq!(take_render_counts(), RenderCounts::default());
    paint(TWO_DIAGRAMS, 80, MermaidMode::Streaming, false);
    assert_eq!(take_render_counts(), RenderCounts::default());
    set_render_counting(true);
}
