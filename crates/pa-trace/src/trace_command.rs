//! `prime-agent trace <traceId|traceparent> [--log <path>] [--json]`: one
//! trace reconstructed from the retained `agent.jsonl` generations (TS
//! `cli/trace-command.ts`). A pure reader over the logging contract: spans
//! are the `span_end` entries, `span_start` entries name spans that have not
//! ended, and every other entry is a log line under the span it carries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pa_types::trace_context::TraceContext;
use serde_json::{Map, Value};

use crate::CommandOutcome;
use crate::record::{SPAN_END_MSG, SPAN_START_MSG, TRACE_COMPONENT};
use crate::retained::{join_paths, js_text, read_log_text, retained_log_files, terminal_safe};

/// Fields rendered structurally; everything else prints as `key=value`.
const RESERVED_LOG_FIELDS: [&str; 7] = [
    "ts",
    "level",
    "component",
    "msg",
    "traceId",
    "spanId",
    "parentSpanId",
];
const MAX_FIELD_VALUE_CHARS: usize = 120;
const MAX_RETAINED_TRACE_LINES: usize = 200_000;
const USAGE: &str = "Usage: prime-agent trace <traceId|traceparent> [--log <path>] [--json]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceCommandOptions {
    pub(crate) trace_id: String,
    pub(crate) log_path: Option<PathBuf>,
    pub(crate) json: bool,
}

/// Parse `<traceId|traceparent> [--log <path>] [--json]`; the error is the
/// usage reason.
pub(crate) fn parse_trace_command_args(args: &[String]) -> Result<TraceCommandOptions, String> {
    let mut target: Option<&str> = None;
    let mut log_path: Option<PathBuf> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--json" {
            json = true;
        } else if arg == "--log" {
            index += 1;
            match args.get(index) {
                Some(path) if !path.starts_with('-') => log_path = Some(PathBuf::from(path)),
                Some(_) | None => return Err("--log requires a path.".to_string()),
            }
        } else if let Some(path) = arg.strip_prefix("--log=") {
            if path.is_empty() {
                return Err("--log requires a path.".to_string());
            }
            log_path = Some(PathBuf::from(path));
        } else if arg.starts_with('-') {
            return Err(format!("Unknown option for trace: {arg}"));
        } else if target.is_none() {
            target = Some(arg);
        } else {
            return Err("trace accepts exactly one trace id or traceparent.".to_string());
        }
        index += 1;
    }
    let target = target.ok_or_else(|| "Missing trace id.".to_string())?;
    Ok(TraceCommandOptions {
        trace_id: normalize_trace_id(target)?,
        log_path,
        json,
    })
}

/// A bare 32-hex trace id (any case) or a full `traceparent`, so whatever
/// the user copied works.
pub(crate) fn normalize_trace_id(value: &str) -> Result<String, String> {
    let lowered = value.trim().to_ascii_lowercase();
    if let Some(context) = TraceContext::parse(&lowered) {
        return Ok(context.trace_id_hex());
    }
    let is_trace_id = lowered.len() == 32
        && lowered
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if is_trace_id && lowered.bytes().any(|byte| byte != b'0') {
        return Ok(lowered);
    }
    Err(format!(
        "Not a trace id or traceparent: {} (expected 32 hex chars or 00-<traceId>-<spanId>-<flags>).",
        Value::from(value)
    ))
}

/// One matching log line: the raw text (for `--json`) and its object.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraceLogLine {
    pub(crate) raw: String,
    pub(crate) entry: Map<String, Value>,
}

/// Every well-formed entry for `trace_id`, in file order (oldest first).
pub(crate) fn read_trace_log_lines(
    files: &[PathBuf],
    trace_id: &str,
) -> std::io::Result<Vec<TraceLogLine>> {
    let mut lines: Vec<TraceLogLine> = Vec::new();
    for file in files {
        let content = read_log_text(file)?;
        for raw in content.split('\n') {
            // A substring test skips the JSON parse for other traces' lines.
            if !raw.contains(trace_id) {
                continue;
            }
            let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let well_formed = entry.get("msg").is_some_and(Value::is_string)
                && entry.get("component").is_some_and(Value::is_string);
            if well_formed && entry.get("traceId").and_then(Value::as_str) == Some(trace_id) {
                lines.push(TraceLogLine {
                    raw: raw.to_string(),
                    entry,
                });
            }
        }
        if lines.len() > MAX_RETAINED_TRACE_LINES {
            lines.drain(..lines.len() - MAX_RETAINED_TRACE_LINES);
        }
    }
    Ok(lines)
}

fn str_field<'a>(entry: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

fn is_span_record(entry: &Map<String, Value>, msg: &str) -> bool {
    str_field(entry, "component") == Some(TRACE_COMPONENT)
        && str_field(entry, "msg") == Some(msg)
        && entry.get("spanId").is_some_and(Value::is_string)
        && entry.get("name").is_some_and(Value::is_string)
}

/// `Date.parse(entry.ts)`, 0 when it is missing or invalid.
fn timestamp_ms(entry: &Map<String, Value>) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "epoch milliseconds are far below 2^53"
    )]
    str_field(entry, "ts")
        .and_then(pa_types::incident::timestamp_to_ms)
        .map_or(0.0, |ms| ms as f64)
}

/// One span of the tree; `end` is `None` for a placeholder (still running,
/// rotated away, or owned by an external caller).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpanNode {
    pub(crate) span_id: String,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) name: String,
    pub(crate) end: Option<Map<String, Value>>,
    /// Epoch ms used only to order siblings.
    pub(crate) start_ms: f64,
    pub(crate) logs: Vec<TraceLogLine>,
    pub(crate) children: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraceTree {
    pub(crate) trace_id: String,
    /// Nodes in first-reference order; `roots` and `children` index into it.
    pub(crate) nodes: Vec<SpanNode>,
    pub(crate) roots: Vec<usize>,
    /// Lines that carry the trace id but no span id.
    pub(crate) unattributed: Vec<TraceLogLine>,
    pub(crate) span_count: usize,
    pub(crate) log_count: usize,
}

struct TreeBuilder {
    nodes: Vec<SpanNode>,
    by_id: HashMap<String, usize>,
}

impl TreeBuilder {
    /// The node for `span_id`, created as an open placeholder when unseen.
    fn placeholder(&mut self, span_id: &str, parent_span_id: Option<&str>, at_ms: f64) -> usize {
        if let Some(&index) = self.by_id.get(span_id) {
            let node = &mut self.nodes[index];
            if node.end.is_none() {
                if node.parent_span_id.is_none() {
                    node.parent_span_id = parent_span_id.map(str::to_string);
                }
                node.start_ms = node.start_ms.min(at_ms);
            }
            return index;
        }
        self.nodes.push(SpanNode {
            span_id: span_id.to_string(),
            parent_span_id: parent_span_id.map(str::to_string),
            name: "(open span)".to_string(),
            end: None,
            start_ms: at_ms,
            logs: Vec::new(),
            children: Vec::new(),
        });
        self.by_id.insert(span_id.to_string(), self.nodes.len() - 1);
        self.nodes.len() - 1
    }
}

/// Build the span tree. Placeholders stand in for spans referenced (as a
/// parent, or by a log line) without a `span_end`, since a trace is usually
/// inspected while its turn is still running.
#[expect(
    clippy::too_many_lines,
    reason = "the TS builder's four passes, kept in order"
)]
pub(crate) fn build_trace_tree(trace_id: &str, lines: &[TraceLogLine]) -> TraceTree {
    let mut builder = TreeBuilder {
        nodes: Vec::new(),
        by_id: HashMap::new(),
    };
    for line in lines {
        let entry = &line.entry;
        if !is_span_record(entry, SPAN_START_MSG) {
            continue;
        }
        let span_id = str_field(entry, "spanId").unwrap_or_default();
        let parent = str_field(entry, "parentSpanId");
        let index = builder.placeholder(span_id, parent, timestamp_ms(entry));
        let node = &mut builder.nodes[index];
        node.name = format!("(open) {}", str_field(entry, "name").unwrap_or_default());
        node.parent_span_id = parent.map(str::to_string);
        node.start_ms = timestamp_ms(entry);
    }
    let mut span_count = 0;
    for line in lines {
        let entry = &line.entry;
        if !is_span_record(entry, SPAN_END_MSG) {
            continue;
        }
        let span_id = str_field(entry, "spanId").unwrap_or_default();
        let parent = str_field(entry, "parentSpanId");
        let duration = entry
            .get("durationMs")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let start = timestamp_ms(entry) - duration;
        let index = builder.placeholder(span_id, parent, start);
        let node = &mut builder.nodes[index];
        if node.end.is_some() {
            continue;
        }
        span_count += 1;
        node.name = str_field(entry, "name").unwrap_or_default().to_string();
        node.end = Some(entry.clone());
        node.parent_span_id = parent.map(str::to_string);
        node.start_ms = start;
    }

    let mut unattributed = Vec::new();
    let mut log_count = 0;
    for line in lines {
        let entry = &line.entry;
        if is_span_record(entry, SPAN_END_MSG) || is_span_record(entry, SPAN_START_MSG) {
            continue;
        }
        log_count += 1;
        let Some(span_id) = str_field(entry, "spanId") else {
            unattributed.push(line.clone());
            continue;
        };
        let index = builder.placeholder(
            span_id,
            str_field(entry, "parentSpanId"),
            timestamp_ms(entry),
        );
        builder.nodes[index].logs.push(line.clone());
    }

    // A parent that never ended still groups its children.
    let mut index = 0;
    while index < builder.nodes.len() {
        let node = &builder.nodes[index];
        if let Some(parent) = node.parent_span_id.clone() {
            if parent != node.span_id {
                let at = node.start_ms;
                builder.placeholder(&parent, None, at);
            }
        }
        index += 1;
    }

    let TreeBuilder { mut nodes, by_id } = builder;
    let mut roots = Vec::new();
    for index in 0..nodes.len() {
        let parent = nodes[index]
            .parent_span_id
            .as_ref()
            .and_then(|parent| by_id.get(parent).copied())
            .filter(|parent| *parent != index);
        match parent {
            Some(parent) => nodes[parent].children.push(index),
            None => roots.push(index),
        }
    }
    let starts: Vec<f64> = nodes.iter().map(|node| node.start_ms).collect();
    let by_start = |left: &usize, right: &usize| starts[*left].total_cmp(&starts[*right]);
    for node in &mut nodes {
        node.children.sort_by(by_start);
        node.logs.sort_by(|left, right| {
            timestamp_ms(&left.entry).total_cmp(&timestamp_ms(&right.entry))
        });
    }
    roots.sort_by(by_start);
    unattributed
        .sort_by(|left, right| timestamp_ms(&left.entry).total_cmp(&timestamp_ms(&right.entry)));
    TraceTree {
        trace_id: trace_id.to_string(),
        nodes,
        roots,
        unattributed,
        span_count,
        log_count,
    }
}

fn format_value(value: &Value) -> String {
    let text = terminal_safe(&js_text(value));
    if text.chars().count() > MAX_FIELD_VALUE_CHARS {
        let head: String = text.chars().take(MAX_FIELD_VALUE_CHARS - 1).collect();
        format!("{head}…")
    } else {
        text
    }
}

fn format_fields(fields: &Map<String, Value>, skip: &[&str]) -> String {
    fields
        .iter()
        .filter(|(key, _)| !skip.contains(&key.as_str()))
        .map(|(key, value)| format!("{key}={}", format_value(value)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_time(entry: &Map<String, Value>) -> String {
    match entry.get("ts") {
        Some(Value::String(ts)) if ts.chars().count() >= 23 => {
            ts.chars().skip(11).take(12).collect()
        }
        Some(Value::Null) | None => "?".to_string(),
        Some(other) => js_text(other),
    }
}

fn format_span_heading(node: &SpanNode) -> String {
    let Some(end) = &node.end else {
        return format!("{} {}", node.name, node.span_id);
    };
    let duration = match end.get("durationMs") {
        Some(value @ Value::Number(_)) => format!("{}ms", js_text(value)),
        _ => "?ms".to_string(),
    };
    let status = match end.get("status") {
        Some(Value::Null) | None => "?".to_string(),
        Some(value) => js_text(value),
    };
    let mut parts = vec![node.name.clone(), duration, status];
    if let Some(Value::Object(attrs)) = end.get("attrs") {
        let rendered = format_fields(attrs, &[]);
        if !rendered.is_empty() {
            parts.push(rendered);
        }
    }
    if let Some(Value::String(error)) = end.get("error") {
        if !error.is_empty() {
            parts.push(format!(
                "error={}",
                format_value(&Value::from(error.as_str()))
            ));
        }
    }
    parts.push(format!("[{}]", node.span_id));
    parts.join("  ")
}

fn format_log_line(line: &TraceLogLine) -> String {
    let entry = &line.entry;
    let level = match entry.get("level") {
        Some(Value::Null) | None => "?".to_string(),
        Some(value) => js_text(value),
    };
    let mut parts = vec![
        format_time(entry),
        format!("{level:<5}"),
        format_value(entry.get("component").unwrap_or(&Value::Null)),
        format_value(entry.get("msg").unwrap_or(&Value::Null)),
    ];
    let extra = format_fields(entry, &RESERVED_LOG_FIELDS);
    if !extra.is_empty() {
        parts.push(extra);
    }
    parts.join("  ")
}

/// One row of the tree: a span (with its subtree) or a log line, ordered by
/// time among its siblings.
enum TreeItem<'a> {
    Span(usize),
    Log(&'a TraceLogLine),
    Unattributed,
}

fn branch(prefix: &str, last: bool) -> String {
    format!("{prefix}{}", if last { "└─ " } else { "├─ " })
}

fn child_prefix(prefix: &str, last: bool) -> String {
    format!("{prefix}{}", if last { "   " } else { "│  " })
}

fn item_time(tree: &TraceTree, item: &TreeItem<'_>) -> f64 {
    match item {
        TreeItem::Span(index) => tree.nodes[*index].start_ms,
        TreeItem::Log(line) => timestamp_ms(&line.entry),
        TreeItem::Unattributed => f64::INFINITY,
    }
}

fn render_items(
    tree: &TraceTree,
    mut items: Vec<TreeItem<'_>>,
    prefix: &str,
    out: &mut Vec<String>,
) {
    items.sort_by(|left, right| item_time(tree, left).total_cmp(&item_time(tree, right)));
    let count = items.len();
    for (position, item) in items.into_iter().enumerate() {
        let last = position + 1 == count;
        match item {
            TreeItem::Log(line) => {
                out.push(format!("{}{}", branch(prefix, last), format_log_line(line)));
            }
            TreeItem::Span(index) => {
                let node = &tree.nodes[index];
                out.push(format!(
                    "{}{}",
                    branch(prefix, last),
                    format_span_heading(node)
                ));
                // Logs and child spans interleave by time so a span reads as a timeline.
                let children: Vec<TreeItem<'_>> = node
                    .logs
                    .iter()
                    .map(TreeItem::Log)
                    .chain(node.children.iter().map(|child| TreeItem::Span(*child)))
                    .collect();
                render_items(tree, children, &child_prefix(prefix, last), out);
            }
            TreeItem::Unattributed => {
                out.push(format!("{}(no span)", branch(prefix, last)));
                let lines = tree.unattributed.iter().map(TreeItem::Log).collect();
                render_items(tree, lines, &child_prefix(prefix, last), out);
            }
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// The tree as text; deterministic for a given log so it can be diffed.
pub(crate) fn format_trace_tree(tree: &TraceTree, files: &[PathBuf]) -> String {
    let mut out = vec![format!(
        "trace {}  ({}, {}, {})",
        tree.trace_id,
        plural(tree.span_count, "span"),
        plural(tree.log_count, "log line"),
        join_paths(files)
    )];
    let mut items: Vec<TreeItem<'_>> = tree
        .roots
        .iter()
        .map(|root| TreeItem::Span(*root))
        .collect();
    if !tree.unattributed.is_empty() {
        // Always last: these lines have no span, so no place on the timeline.
        items.push(TreeItem::Unattributed);
    }
    render_items(tree, items, "", &mut out);
    out.join("\n")
}

/// Run the command; `default_log` is the agent log used without `--log`.
#[must_use]
pub fn run_trace_command(args: &[String], default_log: &Path) -> CommandOutcome {
    let options = match parse_trace_command_args(args) {
        Ok(options) => options,
        Err(reason) => {
            return CommandOutcome::failure(vec![format!("Error: {reason}"), USAGE.to_string()]);
        }
    };
    let log_path = options
        .log_path
        .clone()
        .unwrap_or_else(|| default_log.to_path_buf());
    let files = retained_log_files(&log_path);
    if files.is_empty() {
        return CommandOutcome::failure(vec![format!(
            "Error: no log file at {}",
            log_path.display()
        )]);
    }
    let lines = match read_trace_log_lines(&files, &options.trace_id) {
        Ok(lines) => lines,
        Err(error) => {
            return CommandOutcome::failure(vec![format!(
                "Error: could not read {}: {error}",
                join_paths(&files)
            )]);
        }
    };
    if lines.is_empty() {
        return CommandOutcome::failure(vec![format!(
            "Error: no entries for trace {} in {}",
            options.trace_id,
            join_paths(&files)
        )]);
    }
    if options.json {
        return CommandOutcome::success(lines.into_iter().map(|line| line.raw).collect());
    }
    let tree = build_trace_tree(&options.trace_id, &lines);
    CommandOutcome::success(vec![format_trace_tree(&tree, &files)])
}

#[cfg(test)]
mod tests;
