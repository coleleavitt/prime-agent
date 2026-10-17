//! The status line (Claude Code's `statusLine` setting): a command that
//! reads a JSON description of the session on stdin and prints the rows
//! shown under the prompt.
//!
//! Nothing here runs on the paint path. The run loop offers the current
//! payload every iteration ([`StatusLine::observe`]); a changed payload
//! arms a short throttle, the loop's timer arm launches the command on a
//! tokio task ([`StatusLine::fire`]), and the finished run comes back over
//! a channel ([`StatusLine::apply`]). A new run aborts the previous one, a
//! run is bounded by a timeout, and the last output stays on screen until
//! a newer run replaces it (a failed run clears it, as in Claude Code).

use std::path::PathBuf;
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::client_settings::StatusLineSettings;
use crate::{Line, Span};

/// Claude Code's coalescing window: payload changes inside it share a run.
const THROTTLE: Duration = Duration::from_millis(300);
/// Claude Code's bound on one status line command.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// One finished run, tagged with the launch it answers.
#[derive(Debug)]
pub(crate) struct StatusLineRun {
    generation: u64,
    text: Option<String>,
}

/// The session facts a payload is built from (all already in the client:
/// the status line never asks the daemon for anything).
pub(crate) struct PayloadFacts<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) transcript_path: Option<&'a str>,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) chrome: &'a crate::chrome::ChromeState,
    pub(crate) vim_mode: Option<crate::editor::VimMode>,
}

/// The facts fixed for the run: the launch's extra directories and the
/// output style name, and when the run started.
#[derive(Debug, Clone)]
pub(crate) struct RunFacts {
    pub(crate) added_dirs: Vec<PathBuf>,
    pub(crate) output_style: String,
    pub(crate) started: Instant,
}

/// Claude Code's status line payload (the fields this client knows; the
/// shape of `TPo`): session identity, model, workspace, version, output
/// style, cost, context window, effort, vim mode.
#[must_use]
pub(crate) fn payload(facts: &PayloadFacts<'_>, run: &RunFacts, now: Instant) -> Value {
    let chrome = facts.chrome;
    let cwd = facts.cwd.display().to_string();
    let model_id = chrome.model_id.clone().unwrap_or_default();
    let mut payload = serde_json::json!({
        "session_id": facts.session_id,
        "transcript_path": facts.transcript_path.unwrap_or_default(),
        "cwd": cwd,
        "model": { "id": model_id, "display_name": model_id },
        "workspace": {
            "current_dir": cwd,
            "project_dir": cwd,
            "added_dirs": run
                .added_dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>(),
        },
        "version": chrome.version,
        "output_style": { "name": run.output_style },
        "cost": {
            "total_cost_usd": chrome.cost_usd.unwrap_or(0.0),
            "total_duration_ms": u64::try_from(now.duration_since(run.started).as_millis()).unwrap_or(u64::MAX),
        },
    });
    if let Some(context) = &chrome.context {
        let used = context.percent();
        payload["context_window"] = serde_json::json!({
            "total_input_tokens": context.tokens,
            "context_window_size": context.context_window,
            "used_percentage": used,
            "remaining_percentage": (100.0 - used).max(0.0),
        });
        payload["exceeds_200k_tokens"] = serde_json::json!(context.tokens > 200_000);
    }
    if let Some(level) = &chrome.thinking_suffix {
        payload["effort"] = serde_json::json!({ "level": level });
    }
    if let Some(mode) = facts.vim_mode {
        payload["vim"] = serde_json::json!({ "mode": mode.wire_name() });
    }
    payload
}

/// What a re-run keys on (Claude Code re-runs on token usage, model,
/// effort, vim mode and a new reply, never on the clock).
#[must_use]
pub(crate) fn trigger(facts: &PayloadFacts<'_>) -> Value {
    let chrome = facts.chrome;
    serde_json::json!([
        facts.session_id,
        facts.transcript_path,
        chrome.model_id,
        chrome.thinking_suffix,
        chrome
            .context
            .as_ref()
            .map(|context| (context.tokens, context.context_window)),
        chrome.cost_usd,
        facts.vim_mode.map(crate::editor::VimMode::wire_name),
    ])
}

/// The status line controller the run loop owns.
pub(crate) struct StatusLine {
    settings: Option<StatusLineSettings>,
    cwd: PathBuf,
    run_facts: RunFacts,
    trigger: Option<Value>,
    observed: Option<Value>,
    due: Option<Instant>,
    last_launch: Option<Instant>,
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
    tx: mpsc::UnboundedSender<StatusLineRun>,
}

impl StatusLine {
    /// A controller for `settings` (`None`: the status line is off and
    /// never runs anything) and the channel its runs report on.
    pub(crate) fn new(
        settings: Option<StatusLineSettings>,
        cwd: PathBuf,
        run_facts: RunFacts,
    ) -> (Self, mpsc::UnboundedReceiver<StatusLineRun>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                settings,
                cwd,
                run_facts,
                trigger: None,
                observed: None,
                due: None,
                last_launch: None,
                generation: 0,
                task: None,
                tx,
            },
            rx,
        )
    }

    /// Offer the session's facts: the payload a run would send is kept
    /// fresh, and a changed trigger arms a run.
    pub(crate) fn observe_facts(&mut self, facts: &PayloadFacts<'_>, now: Instant) {
        if self.settings.is_none() {
            return;
        }
        let trigger = trigger(facts);
        let payload = payload(facts, &self.run_facts, now);
        if self.trigger.as_ref() == Some(&trigger) {
            self.observed = Some(payload);
            return;
        }
        self.trigger = Some(trigger);
        self.observe(payload, now);
    }

    #[must_use]
    pub(crate) fn enabled(&self) -> bool {
        self.settings.is_some()
    }

    /// Offer the session's current payload: a change arms a run (the first
    /// one at once, later ones after the throttle window).
    pub(crate) fn observe(&mut self, payload: Value, now: Instant) {
        if self.settings.is_none() || self.observed.as_ref() == Some(&payload) {
            return;
        }
        let first = self.observed.is_none();
        self.observed = Some(payload);
        if self.due.is_none() {
            self.due = Some(if first { now } else { now + THROTTLE });
        }
    }

    /// When the loop must call [`Self::fire`]: an armed change, or the
    /// `refreshInterval` tick after the last launch.
    #[must_use]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let refresh = self
            .settings
            .as_ref()
            .and_then(|settings| settings.refresh_interval)
            .zip(self.last_launch)
            .map(|(interval, last)| last + interval);
        match (self.due, refresh) {
            (Some(due), Some(refresh)) => Some(due.min(refresh)),
            (due, refresh) => due.or(refresh),
        }
    }

    /// Launch the command with the latest payload (aborting a run still in
    /// flight).
    pub(crate) fn fire(&mut self, now: Instant) {
        self.due = None;
        let (Some(settings), Some(payload)) = (&self.settings, &self.observed) else {
            return;
        };
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation += 1;
        self.last_launch = Some(now);
        let generation = self.generation;
        let command = settings.command.clone();
        let input = payload.to_string();
        let cwd = self.cwd.clone();
        let tx = self.tx.clone();
        self.task = Some(tokio::spawn(async move {
            let text = run_command(&command, &input, &cwd).await;
            let _ = tx.send(StatusLineRun { generation, text });
        }));
    }

    /// Fold a finished run: the rows to show now, or `None` for a run a
    /// newer launch superseded.
    pub(crate) fn apply(&mut self, run: StatusLineRun) -> Option<Vec<String>> {
        if run.generation != self.generation {
            return None;
        }
        self.task = None;
        Some(
            run.text
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default(),
        )
    }
}

impl Drop for StatusLine {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Run the command through the platform shell with the payload on stdin:
/// its non-blank stdout lines, or `None` for a failure, a timeout or no
/// output.
async fn run_command(command: &str, input: &str, cwd: &std::path::Path) -> Option<String> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that never reads stdin closes it early: not an error.
        let _ = stdin.write_all(input.as_bytes()).await;
    }
    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout
        .trim()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

// -- rendering -------------------------------------------------------------

fn basic_color(index: u16) -> Color {
    Color::Indexed(u8::try_from(index).unwrap_or(7))
}

/// Apply one SGR parameter list to `style`.
fn apply_sgr(style: Style, params: &str) -> Style {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|code| code.parse().unwrap_or(0))
            .collect()
    };
    let mut style = style;
    let mut index = 0;
    while index < codes.len() {
        let code = codes[index];
        index += 1;
        style = match code {
            0 => Style::default(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            9 => style.add_modifier(Modifier::CROSSED_OUT),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            29 => style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg(basic_color(code - 30)),
            90..=97 => style.fg(basic_color(code - 90 + 8)),
            40..=47 => style.bg(basic_color(code - 40)),
            100..=107 => style.bg(basic_color(code - 100 + 8)),
            39 => style.fg(Color::Reset),
            49 => style.bg(Color::Reset),
            38 | 48 => {
                let color = match codes.get(index) {
                    Some(5) => {
                        let color = codes.get(index + 1).map(|n| basic_color(*n));
                        index += 2;
                        color
                    }
                    Some(2) => {
                        let channel = |offset: usize| {
                            codes
                                .get(index + offset)
                                .and_then(|value| u8::try_from(*value).ok())
                                .unwrap_or(0)
                        };
                        let color = Color::Rgb(channel(1), channel(2), channel(3));
                        index += 4;
                        Some(color)
                    }
                    _ => None,
                };
                match (code, color) {
                    (38, Some(color)) => style.fg(color),
                    (_, Some(color)) => style.bg(color),
                    (_, None) => style,
                }
            }
            _ => style,
        };
    }
    style
}

/// One row of command output as styled spans: SGR colors and attributes
/// apply, every other escape sequence (cursor moves, OSC 8 hyperlinks) is
/// dropped, and control characters never reach the terminal.
fn parse_ansi_row(row: &str) -> Line {
    let mut spans: Line = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = row.chars().peekable();
    let flush = |text: &mut String, style: Style, spans: &mut Line| {
        if !text.is_empty() {
            spans.push(Span::styled(std::mem::take(text), style));
        }
    };
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch == '\t' {
                text.push(' ');
            } else if !ch.is_control() {
                text.push(ch);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut terminator = None;
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        terminator = Some(next);
                        break;
                    }
                    params.push(next);
                }
                if terminator == Some('m') {
                    flush(&mut text, style, &mut spans);
                    style = apply_sgr(style, &params);
                }
            }
            Some(']') => {
                // OSC: up to BEL or ST (`ESC \`).
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut text, style, &mut spans);
    spans
}

/// Cut a row to `width` columns.
fn clip(spans: Line, width: usize) -> Line {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut kept = String::new();
        for ch in span.content.chars() {
            let ch_width = crate::width::char_width(ch);
            if used + ch_width > width {
                break;
            }
            kept.push(ch);
            used += ch_width;
        }
        let whole = kept.len() == span.content.len();
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if !whole {
            break;
        }
    }
    out
}

/// The status rows under the prompt: `padding` columns in, each cut to the
/// frame width.
#[must_use]
pub(crate) fn render_status_rows(rows: &[String], padding: usize, width: usize) -> Vec<Line> {
    rows.iter()
        .map(|row| {
            let mut line: Line = Vec::new();
            let pad = padding.min(width);
            if pad > 0 {
                line.push(Span::raw(" ".repeat(pad)));
            }
            line.extend(clip(parse_ansi_row(row), width - pad));
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(command: &str) -> StatusLineSettings {
        StatusLineSettings {
            command: command.to_string(),
            padding: 0,
            refresh_interval: None,
            hide_vim_mode_indicator: false,
        }
    }

    fn run_facts(started: Instant) -> RunFacts {
        RunFacts {
            added_dirs: vec![PathBuf::from("/extra")],
            output_style: "Explanatory".to_string(),
            started,
        }
    }

    #[tokio::test]
    async fn the_payload_carries_the_claude_code_fields_and_reruns_ignore_the_clock() {
        let started = Instant::now();
        let chrome = crate::chrome::ChromeState {
            version: "1.2.3".to_string(),
            model_id: Some("claude-x".to_string()),
            cost_usd: Some(0.5),
            context: Some(crate::chrome::ContextUsage {
                tokens: 50_000,
                context_window: 200_000,
            }),
            thinking_suffix: Some("high".to_string()),
            ..Default::default()
        };
        let facts = PayloadFacts {
            session_id: "s1",
            transcript_path: Some("/sessions/s1.jsonl"),
            cwd: std::path::Path::new("/work"),
            chrome: &chrome,
            vim_mode: Some(crate::editor::VimMode::Normal),
        };
        assert_eq!(
            payload(
                &facts,
                &run_facts(started),
                started + Duration::from_millis(1500)
            ),
            serde_json::json!({
                "session_id": "s1",
                "transcript_path": "/sessions/s1.jsonl",
                "cwd": "/work",
                "model": { "id": "claude-x", "display_name": "claude-x" },
                "workspace": { "current_dir": "/work", "project_dir": "/work", "added_dirs": ["/extra"] },
                "version": "1.2.3",
                "output_style": { "name": "Explanatory" },
                "cost": { "total_cost_usd": 0.5, "total_duration_ms": 1500 },
                "context_window": {
                    "total_input_tokens": 50_000,
                    "context_window_size": 200_000,
                    "used_percentage": 25.0,
                    "remaining_percentage": 75.0
                },
                "exceeds_200k_tokens": false,
                "effort": { "level": "high" },
                "vim": { "mode": "NORMAL" }
            })
        );
        let dir = tempfile::tempdir().unwrap();
        let (mut status, _rx) = StatusLine::new(
            Some(settings("true")),
            dir.path().to_path_buf(),
            run_facts(started),
        );
        status.observe_facts(&facts, started);
        status.fire(started);
        // Only the clock moved: nothing re-arms.
        status.observe_facts(&facts, started + Duration::from_secs(9));
        assert_eq!(status.deadline(), None);
    }

    #[test]
    fn sgr_rows_become_styled_spans_and_other_escapes_drop() {
        let row = "\u{1b}[1;32mok\u{1b}[0m \u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7} \u{1b}[38;2;1;2;3mrgb\u{1b}[2K";
        assert_eq!(
            parse_ansi_row(row),
            vec![
                Span::styled(
                    "ok",
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Indexed(2))
                ),
                Span::raw(" link "),
                Span::styled("rgb", Style::default().fg(Color::Rgb(1, 2, 3))),
            ]
        );
        assert_eq!(
            render_status_rows(&["abcdef".to_string()], 2, 5),
            vec![vec![Span::raw("  "), Span::raw("abc")]]
        );
    }

    #[tokio::test]
    async fn changes_throttle_and_a_run_reports_the_command_output() {
        let dir = tempfile::tempdir().unwrap();
        let (mut status, mut rx) = StatusLine::new(
            Some(settings(
                "read -r input; printf '%s\\n\\n' \"$input\" | cut -c1-12; echo second",
            )),
            dir.path().to_path_buf(),
            run_facts(Instant::now()),
        );
        let now = Instant::now();
        assert_eq!(status.deadline(), None);
        status.observe(serde_json::json!({ "model": 1 }), now);
        // The first payload runs at once; a change inside the window waits.
        assert_eq!(status.deadline(), Some(now));
        status.fire(now);
        status.observe(serde_json::json!({ "model": 2 }), now);
        status.observe(serde_json::json!({ "model": 3 }), now);
        assert_eq!(status.deadline(), Some(now + THROTTLE));
        let run = rx.recv().await.unwrap();
        assert_eq!(
            status.apply(run),
            Some(vec!["{\"model\":1}".to_string(), "second".to_string()])
        );
        // A superseded run is dropped.
        status.fire(now);
        status.fire(now);
        let mut outputs = Vec::new();
        while outputs.is_empty() {
            let run = rx.recv().await.unwrap();
            outputs.extend(status.apply(run));
        }
        assert_eq!(
            outputs,
            vec![vec!["{\"model\":3}".to_string(), "second".to_string()]]
        );
    }

    #[tokio::test]
    async fn a_failing_command_clears_and_an_unset_status_line_never_arms() {
        let dir = tempfile::tempdir().unwrap();
        let (mut status, mut rx) = StatusLine::new(
            Some(settings("echo x; exit 3")),
            dir.path().to_path_buf(),
            run_facts(Instant::now()),
        );
        status.observe(serde_json::json!({}), Instant::now());
        status.fire(Instant::now());
        let run = rx.recv().await.unwrap();
        assert_eq!(status.apply(run), Some(Vec::new()));
        let (mut off, _rx) =
            StatusLine::new(None, dir.path().to_path_buf(), run_facts(Instant::now()));
        off.observe(serde_json::json!({}), Instant::now());
        assert_eq!(off.deadline(), None);
    }
}
