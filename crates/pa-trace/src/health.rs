//! `prime-agent health [--since <d>] [--stuck-after <d>] [--limit <n>]
//! [--log <path>] [--json]`: a bounded incident summary over the retained
//! `agent.jsonl` generations, without contacting the daemon (TS
//! `cli/health-command.ts`). A retained-log heuristic, not a live probe; it
//! fails closed (exit 2) on incidents and on malformed, empty, or stale
//! evidence.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use pa_types::incident::timestamp_to_ms;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::CommandOutcome;
use crate::record::format_iso_from_ms;
use crate::retained::{join_paths, read_log_text, retained_log_files, terminal_safe};

const DEFAULT_SINCE_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_STUCK_AFTER_MS: i64 = 10 * 60 * 1000;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;
const MAX_HEALTH_ENTRIES: usize = 100_000;
const MAX_OPEN_LIFECYCLE_ENTRIES: usize = 100_000;
/// The `Number.isSafeInteger` bound on a parsed duration.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;
/// The delivered-notice line of an RLM child's terminal notice.
const RLM_CHILD_TERMINAL_NOTICE_DELIVERED_MSG: &str = "rlm_child_terminal_notice_delivered";
const USAGE: &str = "Usage: prime-agent health [--since <duration>] [--stuck-after <duration>] [--limit <n>] [--log <path>] [--json]";

/// Incident classes, in report order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Category {
    Historian,
    Provider,
    StuckTurn,
    DaemonRecovery,
    Process,
    Kernel,
    Child,
    MessageDelivery,
    Lock,
    Orphan,
    Diagnostic,
}

const CATEGORIES: [(Category, &str, &str); 11] = [
    (Category::Historian, "historian", "Historian failures"),
    (Category::Provider, "provider", "Provider errors"),
    (Category::StuckTurn, "stuck_turn", "Stuck turns"),
    (
        Category::DaemonRecovery,
        "daemon_recovery",
        "Daemon recovery failures",
    ),
    (Category::Process, "process", "Process failures"),
    (Category::Kernel, "kernel", "Kernel failures"),
    (Category::Child, "child", "Child failures"),
    (
        Category::MessageDelivery,
        "message_delivery",
        "Agent message delivery failures",
    ),
    (Category::Lock, "lock", "Lock failures"),
    (Category::Orphan, "orphan", "Orphan cleanup failures"),
    (Category::Diagnostic, "diagnostic", "Diagnostic uncertainty"),
];

impl Category {
    fn key(self) -> &'static str {
        CATEGORIES
            .iter()
            .find(|(category, _, _)| *category == self)
            .map_or("diagnostic", |(_, key, _)| key)
    }
}

/// Retained operations whose start without an end is an incident.
fn open_operation_category(name: &str) -> Option<Category> {
    match name {
        "bash.command" => Some(Category::Process),
        "kernel.cell" | "kernel.execute" => Some(Category::Kernel),
        "rlm.child" | "rlm.child.run" | "child.passivate" | "child.delete" => Some(Category::Child),
        "cargo_lock_wait" | "bootstrap_lock_wait" | "kernel.bootstrap_lock" => Some(Category::Lock),
        _ => None,
    }
}

fn is_stuck_turn_span(name: &str) -> bool {
    matches!(name, "client.turn" | "agent.prompt")
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("valid health pattern")
}

// A failing command or a raised Python exception is returned to the model, which reads it and
// reacts: agent-visible tool results, not operator incidents.
static PYTHON_EXCEPTION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Za-z_][\w.]*(?:Error|Exception|Exit|Interrupt|Warning)\b"));
static KERNEL_CANCELLATION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)^(?:interrupted|cancell?ed)\b"));
static AGENT_MESSAGE_BACKPRESSURE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)too many pending messages"));
static DURATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(\d+)(ms|s|m|h|d)$"));
static DAEMON_RECOVERY: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)\b(recover(?:y|ing|ed)?|restart(?:ed|ing)?)\b"));
static DAEMON_FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\b(fail(?:ed|ure)?|interrupt(?:ed)?|cancel(?:led)?|could not|did not answer|uncertain)\b",
    )
});
static KERNEL_COMPONENT: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)kernel"));
static FATAL_CRASH: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)fatal_crash"));
static CHILD_COMPONENT: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)child"));
static CHILD_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)(?:timeout|timed out|error|failed)"));
static LOCK_MSG: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)^(?:cargo_lock_wait|bootstrap_lock_wait)$"));
static LOCK_NAME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)(?:cargo_lock_wait|bootstrap_lock_wait)"));
static LOCK_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)(?:error|failed|failure|timeout|timed out)"));
static ORPHAN: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)orphan"));
static ORPHAN_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)(?:corrupt|write|failed|failure|could not|cannot)"));

#[derive(Debug, Clone, PartialEq, Eq)]
struct HealthOptions {
    log_path: Option<PathBuf>,
    since_ms: i64,
    stuck_after_ms: i64,
    limit: usize,
    json: bool,
}

fn parse_duration(value: &str, option: &str) -> Result<i64, String> {
    let captures = DURATION
        .captures(value)
        .ok_or_else(|| format!("{option} requires a duration such as 30m, 6h, or 2d."))?;
    let multiplier: i64 = match &captures[2] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => 86_400_000,
    };
    captures[1]
        .parse::<i64>()
        .ok()
        .and_then(|amount| amount.checked_mul(multiplier))
        .filter(|result| *result > 0 && *result <= MAX_SAFE_INTEGER)
        .ok_or_else(|| format!("{option} must be positive."))
}

fn parse_health_args(args: &[String]) -> Result<HealthOptions, String> {
    let mut options = HealthOptions {
        log_path: None,
        since_ms: DEFAULT_SINCE_MS,
        stuck_after_ms: DEFAULT_STUCK_AFTER_MS,
        limit: DEFAULT_LIMIT,
        json: false,
    };
    let mut limit = pa_types::js::js_number("20");
    let mut log_path: Option<String> = None;
    let take_value = |index: usize, option: &str| -> Result<String, String> {
        match args.get(index + 1) {
            Some(value) if !value.starts_with('-') => Ok(value.clone()),
            Some(_) | None => Err(format!("{option} requires a value.")),
        }
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--json" {
            options.json = true;
        } else if arg == "--log" {
            log_path = Some(take_value(index, "--log")?);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--log=") {
            log_path = Some(value.to_string());
        } else if arg == "--since" {
            options.since_ms = parse_duration(&take_value(index, "--since")?, "--since")?;
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--since=") {
            options.since_ms = parse_duration(value, "--since")?;
        } else if arg == "--stuck-after" {
            options.stuck_after_ms =
                parse_duration(&take_value(index, "--stuck-after")?, "--stuck-after")?;
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--stuck-after=") {
            options.stuck_after_ms = parse_duration(value, "--stuck-after")?;
        } else if arg == "--limit" {
            limit = pa_types::js::js_number(&take_value(index, "--limit")?);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--limit=") {
            limit = pa_types::js::js_number(value);
        } else {
            return Err(format!("Unknown option for health: {arg}"));
        }
        index += 1;
    }
    if log_path.as_deref() == Some("") {
        return Err("--log requires a path.".to_string());
    }
    options.log_path = log_path.map(PathBuf::from);
    #[expect(clippy::cast_precision_loss, reason = "MAX_LIMIT is tiny")]
    let in_range = limit.fract() == 0.0 && limit > 0.0 && limit <= MAX_LIMIT as f64;
    if !in_range {
        return Err(format!("--limit must be an integer from 1 to {MAX_LIMIT}."));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked to be an integer in 1..=200"
    )]
    {
        options.limit = limit as usize;
    }
    Ok(options)
}

/// One parsed entry; `id` is its read order (the TS object identity).
#[derive(Debug, Clone)]
struct HealthEntry {
    id: u64,
    ts: String,
    at_ms: i64,
    fields: Map<String, Value>,
    before_window: bool,
}

impl HealthEntry {
    fn str_field(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }

    fn component(&self) -> &str {
        self.fields
            .get("component")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn msg(&self) -> &str {
        self.fields
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn is_trace(&self, msg: &str) -> bool {
        self.component() == "trace" && self.msg() == msg
    }

    /// `traceId:spanId` when both are present.
    fn span_key(&self) -> Option<String> {
        Some(format!(
            "{}:{}",
            self.str_field("traceId")?,
            self.str_field("spanId")?
        ))
    }

    fn attrs(&self) -> Option<&Map<String, Value>> {
        self.fields.get("attrs").and_then(Value::as_object)
    }

    fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs().and_then(|attrs| attrs.get(key))
    }

    fn attr_str(&self, key: &str) -> Option<&str> {
        self.attr(key).and_then(Value::as_str)
    }
}

struct ReadResult {
    entries: Vec<HealthEntry>,
    parse_errors: u64,
    latest_entry_at: Option<String>,
}

/// Unmatched `span_start` entries in first-seen order, bounded.
#[derive(Default)]
struct OpenLifecycle {
    by_key: HashMap<String, (u64, HealthEntry)>,
    order: BTreeMap<u64, String>,
    next: u64,
}

impl OpenLifecycle {
    /// `Map#set`: an existing key keeps its position. Answers whether the
    /// oldest start was evicted to stay within the bound.
    fn set(&mut self, key: String, entry: HealthEntry) -> bool {
        if let Some(slot) = self.by_key.get_mut(&key) {
            slot.1 = entry;
            return false;
        }
        self.order.insert(self.next, key.clone());
        self.by_key.insert(key, (self.next, entry));
        self.next += 1;
        if self.by_key.len() > MAX_OPEN_LIFECYCLE_ENTRIES {
            if let Some((_, oldest)) = self.order.pop_first() {
                self.by_key.remove(&oldest);
            }
            return true;
        }
        false
    }

    fn delete(&mut self, key: &str) {
        if let Some((sequence, _)) = self.by_key.remove(key) {
            self.order.remove(&sequence);
        }
    }

    fn into_entries(mut self) -> Vec<HealthEntry> {
        let order = std::mem::take(&mut self.order);
        order
            .into_values()
            .filter_map(|key| self.by_key.remove(&key).map(|(_, entry)| entry))
            .collect()
    }
}

fn read_health_entries(files: &[PathBuf], cutoff_ms: i64) -> std::io::Result<ReadResult> {
    let mut entries: VecDeque<HealthEntry> = VecDeque::new();
    let mut open = OpenLifecycle::default();
    let mut parse_errors = 0;
    let mut latest: Option<(i64, String)> = None;
    let mut next_id = 0;
    for file in files {
        let content = read_log_text(file)?;
        for raw in content.split('\n') {
            if raw.trim().is_empty() {
                continue;
            }
            let fields = match serde_json::from_str::<Value>(raw) {
                Ok(Value::Object(fields))
                    if ["ts", "component", "msg"]
                        .iter()
                        .all(|key| fields.get(*key).is_some_and(Value::is_string)) =>
                {
                    fields
                }
                _ => {
                    parse_errors += 1;
                    continue;
                }
            };
            let ts = fields
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(at_ms) = timestamp_to_ms(&ts) else {
                parse_errors += 1;
                continue;
            };
            if latest
                .as_ref()
                .is_none_or(|(latest_ms, _)| at_ms > *latest_ms)
            {
                latest = Some((at_ms, ts.clone()));
            }
            let entry = HealthEntry {
                id: next_id,
                ts,
                at_ms,
                fields,
                before_window: false,
            };
            next_id += 1;
            if let Some(key) = entry.span_key() {
                if entry.is_trace("span_start") && open.set(key.clone(), entry.clone()) {
                    parse_errors += 1;
                }
                if entry.is_trace("span_end") {
                    open.delete(&key);
                }
            }
            if at_ms < cutoff_ms {
                continue;
            }
            entries.push_back(entry);
            if entries.len() > MAX_HEALTH_ENTRIES {
                entries.pop_front();
            }
        }
    }
    // Unmatched starts survive independently of the bounded buffer, so a
    // high-volume log cannot evict a genuinely open operation.
    let present: HashSet<u64> = entries.iter().map(|entry| entry.id).collect();
    let mut entries: Vec<HealthEntry> = entries.into();
    for mut entry in open.into_entries() {
        if !present.contains(&entry.id) {
            entry.before_window = entry.at_ms < cutoff_ms;
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.at_ms);
    Ok(ReadResult {
        entries,
        parse_errors,
        latest_entry_at: latest.map(|(_, ts)| ts),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Incident {
    category: Category,
    ts: String,
    at_ms: i64,
    summary: String,
    trace_id: Option<String>,
    session_id: Option<String>,
}

fn incident(category: Category, entry: &HealthEntry, summary: String) -> Incident {
    let session_id = entry
        .str_field("sessionId")
        .or_else(|| entry.attr_str("session.id"))
        .or_else(|| entry.attr_str("historian.session_id"))
        .filter(|session| !session.is_empty())
        .map(str::to_string);
    Incident {
        category,
        ts: entry.ts.clone(),
        at_ms: entry.at_ms,
        summary,
        trace_id: entry.str_field("traceId").map(str::to_string),
        session_id,
    }
}

fn span_failed(entry: &HealthEntry) -> bool {
    entry.fields.get("status").and_then(Value::as_str) == Some("error")
        || entry.attr("historian.valid") == Some(&Value::Bool(false))
        || matches!(
            entry.attr_str("historian.outcome"),
            Some("failed" | "failure" | "error")
        )
}

fn detail(entry: &HealthEntry, fallback: &str) -> String {
    entry
        .str_field("error")
        .or_else(|| entry.str_field("message"))
        .or_else(|| entry.attr_str("error"))
        .or_else(|| entry.attr_str("historian.failure_reason"))
        .unwrap_or(fallback)
        .to_string()
}

/// The failure was a tool result the agent read and could react to: volume,
/// not an incident. A bash command with no exit status never ran, which the
/// agent cannot fix, so it stays an incident.
fn agent_visible_tool_failure(name: &str, entry: &HealthEntry) -> bool {
    if name == "bash.command" {
        return entry.attr("bash.exit_code").is_some();
    }
    if name != "kernel.cell" && name != "kernel.execute" {
        return false;
    }
    let failure = detail(entry, "");
    KERNEL_CANCELLATION.is_match(&failure) || PYTHON_EXCEPTION.is_match(&failure)
}

fn format_duration(ms: i64) -> String {
    if ms >= 86_400_000 {
        format!("{}d", ms / 86_400_000)
    } else if ms >= 3_600_000 {
        format!("{}h", ms / 3_600_000)
    } else if ms >= 60_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

/// The summary in the TS JSON shape.
#[derive(Debug, Clone, PartialEq)]
struct HealthSummary {
    status: &'static str,
    generated_at: String,
    since: String,
    files: Vec<String>,
    counts: Vec<(Category, u64)>,
    tool_errors: Vec<(Category, u64)>,
    incidents: Vec<Incident>,
    truncated: bool,
    parse_errors: u64,
    stale: bool,
    latest_entry_at: Option<String>,
}

fn empty_counts() -> Vec<(Category, u64)> {
    CATEGORIES
        .iter()
        .map(|(category, _, _)| (*category, 0))
        .collect()
}

fn bump(counts: &mut [(Category, u64)], category: Category) {
    if let Some(slot) = counts
        .iter_mut()
        .find(|(candidate, _)| *candidate == category)
    {
        slot.1 += 1;
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one pass over the TS classifier, kept in source order"
)]
fn summarize(
    read: &ReadResult,
    now_ms: i64,
    options: &HealthOptions,
    files: &[PathBuf],
    since_ms: i64,
) -> HealthSummary {
    let entries = &read.entries;
    let mut incidents: Vec<Incident> = Vec::new();
    let mut tool_errors = empty_counts();
    let provider_log_spans: HashSet<String> = entries
        .iter()
        .filter(|entry| {
            entry.component() == "ai.provider" && entry.msg() == "provider stream failure"
        })
        .filter_map(HealthEntry::span_key)
        .collect();
    let ended_spans: HashSet<String> = entries
        .iter()
        .filter(|entry| entry.is_trace("span_end"))
        .filter_map(HealthEntry::span_key)
        .collect();
    let active_starts: Vec<&HealthEntry> = entries
        .iter()
        .filter(|entry| {
            let name = entry.str_field("name").unwrap_or_default();
            entry.is_trace("span_start")
                && entry.fields.get("spanId").is_some_and(Value::is_string)
                && (is_stuck_turn_span(name) || open_operation_category(name).is_some())
                && !ended_spans.contains(&entry.span_key().unwrap_or_default())
        })
        .collect();

    for entry in entries.iter().filter(|entry| !entry.before_window) {
        let name = entry.str_field("name");
        let component = entry.component();
        let msg = entry.msg();
        let span_end = entry.is_trace("span_end");
        if span_end && name.is_some_and(|name| name.starts_with("historian.")) && span_failed(entry)
        {
            incidents.push(incident(
                Category::Historian,
                entry,
                format!("{}: {}", name.unwrap_or_default(), detail(entry, "failed")),
            ));
        }
        let provider_log = component == "ai.provider" && msg == "provider stream failure";
        let provider_span = span_end
            && name == Some("llm.request")
            && entry.fields.get("status").and_then(Value::as_str) == Some("error");
        let paired = entry
            .span_key()
            .is_some_and(|key| provider_log_spans.contains(&key));
        if provider_log || (provider_span && !paired) {
            let provider = entry
                .str_field("provider")
                .or_else(|| entry.attr_str("llm.provider"))
                .unwrap_or("provider");
            incidents.push(incident(
                Category::Provider,
                entry,
                format!("{provider}: {}", detail(entry, "request failed")),
            ));
        }
        if let Some(name) = name.filter(|_| span_end && span_failed(entry)) {
            let category = if name.starts_with("child.") {
                Some(Category::Child)
            } else {
                open_operation_category(name)
            };
            if let Some(category) = category {
                if agent_visible_tool_failure(name, entry) {
                    bump(&mut tool_errors, category);
                } else {
                    incidents.push(incident(
                        category,
                        entry,
                        format!("{name}: {}", detail(entry, "failed")),
                    ));
                }
            }
        }
        if span_end && name == Some("kernel.host_request") && span_failed(entry) {
            // A rejected agent message is work that never reached its reader.
            let failure = detail(entry, "host request failed");
            let request_type = entry
                .attr_str("host_request.type")
                .unwrap_or("host request");
            if AGENT_MESSAGE_BACKPRESSURE.is_match(&failure) {
                incidents.push(incident(
                    Category::MessageDelivery,
                    entry,
                    format!("{request_type}: {failure}"),
                ));
            } else {
                bump(&mut tool_errors, Category::Kernel);
            }
        }
        if msg == RLM_CHILD_TERMINAL_NOTICE_DELIVERED_MSG {
            // A cancellation notice is an operator action, not an incident.
            if let Some(kind) = entry
                .str_field("kind")
                .filter(|kind| matches!(*kind, "completed_without_reply" | "failure"))
            {
                let child_id = entry.str_field("rlm.child_id").unwrap_or("unknown");
                incidents.push(incident(
                    Category::Child,
                    entry,
                    format!("rlm child {child_id}: {kind} notice delivered to parent"),
                ));
            }
        }
        if msg == "kernel_exit" && KERNEL_COMPONENT.is_match(component) {
            incidents.push(incident(
                Category::Kernel,
                entry,
                detail(entry, "kernel exited unexpectedly"),
            ));
        }
        if FATAL_CRASH.is_match(msg) {
            incidents.push(incident(Category::Process, entry, msg.to_string()));
        }
        if CHILD_COMPONENT.is_match(component) && CHILD_FAILURE.is_match(msg) {
            incidents.push(incident(Category::Child, entry, msg.to_string()));
        }
        if component != "trace"
            && (LOCK_MSG.is_match(msg)
                || (LOCK_NAME.is_match(&format!("{} {msg}", name.unwrap_or_default()))
                    && LOCK_FAILURE.is_match(msg)))
        {
            incidents.push(incident(Category::Lock, entry, msg.to_string()));
        }
        let orphan_failed = entry
            .attr("outcome")
            .or_else(|| entry.fields.get("outcome"))
            .and_then(Value::as_str)
            == Some("failed");
        if ORPHAN.is_match(&format!("{component} {msg}"))
            && (ORPHAN_FAILURE.is_match(msg) || orphan_failed)
        {
            let suffix = if orphan_failed { ": failed" } else { "" };
            incidents.push(incident(Category::Orphan, entry, format!("{msg}{suffix}")));
        }
        if component.contains("daemon")
            && DAEMON_RECOVERY.is_match(msg)
            && DAEMON_FAILURE.is_match(msg)
        {
            incidents.push(incident(Category::DaemonRecovery, entry, msg.to_string()));
        }
    }

    // `Map` keyed by span key, later starts replacing earlier ones in place.
    let mut stuck: Vec<(Option<String>, &HealthEntry)> = Vec::new();
    let mut stuck_index: HashMap<Option<String>, usize> = HashMap::new();
    for entry in active_starts {
        let key = entry.span_key();
        if let Some(&index) = stuck_index.get(&key) {
            stuck[index].1 = entry;
        } else {
            stuck_index.insert(key.clone(), stuck.len());
            stuck.push((key, entry));
        }
    }
    for (key, entry) in stuck {
        let age_ms = now_ms - entry.at_ms;
        if age_ms >= options.stuck_after_ms {
            let name = entry.str_field("name").unwrap_or("operation");
            let category = if is_stuck_turn_span(name) {
                Category::StuckTurn
            } else {
                open_operation_category(name).unwrap_or(Category::Process)
            };
            let span = entry
                .fields
                .get("spanId")
                .and_then(Value::as_str)
                .map_or_else(|| key.unwrap_or_default(), str::to_string);
            incidents.push(incident(
                category,
                entry,
                format!(
                    "{name} span {span} has no completion after {}",
                    format_duration(age_ms)
                ),
            ));
        }
    }
    incidents.sort_by_key(|item| std::cmp::Reverse(item.at_ms));
    let mut kept: Vec<Incident> = Vec::new();
    for item in incidents {
        let duplicate_orphan = item.category == Category::Orphan
            && kept.iter().any(|candidate| {
                candidate.category == Category::Orphan
                    && candidate.trace_id == item.trace_id
                    && candidate.session_id == item.session_id
                    && candidate.summary == item.summary
            });
        if !duplicate_orphan {
            kept.push(item);
        }
    }
    let mut counts = empty_counts();
    for item in &kept {
        bump(&mut counts, item.category);
    }
    let stale = read
        .latest_entry_at
        .as_deref()
        .and_then(timestamp_to_ms)
        .is_none_or(|latest| latest < since_ms);
    let unknown = read.parse_errors > 0 || stale;
    if read.parse_errors > 0 {
        bump(&mut counts, Category::Diagnostic);
    }
    if stale {
        bump(&mut counts, Category::Diagnostic);
    }
    let status = if !kept.is_empty() {
        "unhealthy"
    } else if unknown {
        "unknown"
    } else {
        "healthy"
    };
    let truncated = kept.len() > options.limit;
    kept.truncate(options.limit);
    HealthSummary {
        status,
        generated_at: format_iso_from_ms(now_ms),
        since: format_iso_from_ms(since_ms),
        files: files
            .iter()
            .map(|file| file.display().to_string())
            .collect(),
        counts,
        tool_errors,
        incidents: kept,
        truncated,
        parse_errors: read.parse_errors,
        stale,
        latest_entry_at: read.latest_entry_at.clone(),
    }
}

fn counts_json(counts: &[(Category, u64)]) -> Value {
    Value::Object(
        counts
            .iter()
            .map(|(category, count)| (category.key().to_string(), Value::from(*count)))
            .collect(),
    )
}

fn summary_json(summary: &HealthSummary) -> Value {
    let incidents: Vec<Value> = summary
        .incidents
        .iter()
        .map(|item| {
            let mut object = Map::new();
            object.insert("category".to_string(), Value::from(item.category.key()));
            object.insert("ts".to_string(), Value::from(item.ts.as_str()));
            object.insert("summary".to_string(), Value::from(item.summary.as_str()));
            if let Some(trace_id) = &item.trace_id {
                object.insert("traceId".to_string(), Value::from(trace_id.as_str()));
            }
            if let Some(session_id) = &item.session_id {
                object.insert("sessionId".to_string(), Value::from(session_id.as_str()));
            }
            Value::Object(object)
        })
        .collect();
    let mut out = json!({
        "status": summary.status,
        "generatedAt": summary.generated_at,
        "since": summary.since,
        "files": summary.files,
        "counts": counts_json(&summary.counts),
        "toolErrors": counts_json(&summary.tool_errors),
        "incidents": incidents,
        "truncated": summary.truncated,
        "parseErrors": summary.parse_errors,
        "stale": summary.stale,
    });
    if let (Some(latest), Value::Object(object)) = (&summary.latest_entry_at, &mut out) {
        object.insert("latestEntryAt".to_string(), Value::from(latest.as_str()));
    }
    out
}

fn format_summary(summary: &HealthSummary) -> String {
    let total: u64 = summary.counts.iter().map(|(_, count)| count).sum();
    let mut out = vec![format!(
        "health since {}  ({total} incident{}; {})",
        summary.since,
        if total == 1 { "" } else { "s" },
        summary.files.join(", ")
    )];
    for (category, _, label) in CATEGORIES {
        let count = summary
            .counts
            .iter()
            .find(|(candidate, _)| *candidate == category)
            .map_or(0, |(_, count)| *count);
        out.push(format!("{label}: {count}"));
        for item in summary
            .incidents
            .iter()
            .filter(|item| item.category == category)
        {
            let context = [
                item.session_id
                    .as_ref()
                    .map(|session| format!("session={session}")),
                item.trace_id.as_ref().map(|trace| format!("trace={trace}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            let context = if context.is_empty() {
                String::new()
            } else {
                format!("  {}", terminal_safe(&context))
            };
            out.push(format!(
                "  {}  {}{context}",
                item.ts,
                terminal_safe(&item.summary)
            ));
        }
    }
    let tool_errors: Vec<String> = summary
        .tool_errors
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(category, count)| format!("{}={count}", category.key()))
        .collect();
    if !tool_errors.is_empty() {
        out.push(format!(
            "Agent-visible tool errors (not incidents): {}",
            tool_errors.join(" ")
        ));
    }
    if summary.parse_errors > 0 {
        out.push(format!(
            "UNKNOWN: {} malformed log line(s) could not be evaluated.",
            summary.parse_errors
        ));
    }
    if summary.stale {
        out.push(format!(
            "UNKNOWN: no valid log entry exists in the selected window (latest={}).",
            summary.latest_entry_at.as_deref().unwrap_or("none")
        ));
    }
    if summary.truncated {
        out.push("Recent incident details truncated; increase --limit to show more.".to_string());
    }
    out.join("\n")
}

/// Run the command at `now_ms`; `default_log` is the agent log used without
/// `--log`. Exit 0 only for valid recent evidence with no incidents, 2 for
/// incidents or unknown evidence, 1 for usage and read errors.
#[must_use]
pub fn run_health_command(args: &[String], default_log: &Path, now_ms: i64) -> CommandOutcome {
    let options = match parse_health_args(args) {
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
    let cutoff_ms = now_ms - options.since_ms;
    let read = match read_health_entries(&files, cutoff_ms) {
        Ok(read) => read,
        Err(error) => {
            return CommandOutcome::failure(vec![format!(
                "Error: could not read {}: {error}",
                join_paths(&files)
            )]);
        }
    };
    let summary = summarize(&read, now_ms, &options, &files, cutoff_ms);
    let text = if options.json {
        serde_json::to_string_pretty(&summary_json(&summary)).unwrap_or_default()
    } else {
        format_summary(&summary)
    };
    CommandOutcome {
        code: if summary.status == "healthy" { 0 } else { 2 },
        stdout: vec![text],
        stderr: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
