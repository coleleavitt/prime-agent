//! `prime-agent learning backfill`: the offline cross-tool backfill generator
//! for the Engineer Trajectory Index (the port of the fork's
//! `skills/trajectory-backfill/backfill.py`, byte-compatible output).
//!
//! Walks the coding-session corpora on this machine (prime, Claude Code,
//! opencode), buckets records by UTC calendar day, and writes one
//! `LearningDay`-shaped file per (corpus, day) that `learning trajectory
//! --include-backfill` folds into its table:
//!
//! ```text
//! <out>/<corpus>/<YYYY-MM-DD>.json   (file mode 0600, dir 0700)
//! ```
//!
//! Each day carries aggregate counts and structural error-class tokens only
//! (class names, well-known failure strings), never message content. Every
//! datum is corpus/tool-tagged with a synthetic fingerprint that cannot
//! collide with a runtime ledger id, so the trajectory reader flags all three
//! confounds on these windows. Nothing here calls a provider or the network,
//! the corpora are only read, and auth files are never opened. The output is
//! never written into `trajectory.json` or the prompt.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};

use crate::json::stringify_pretty;

/// Never opened, even under a corpus root.
const SKIP_NAMES: [&str; 2] = ["auth.json", "anthropic-auth-state.json"];
/// Tokens kept per text blob, so one huge log cannot dominate a day.
const MAX_TOKENS_PER_BLOB: usize = 8;

static EXCEPTION_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z][A-Za-z0-9_]*(?:Error|Exception|Warning))\b").expect("valid regex")
});
static RUSTC_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\berror\[(E\d{2,4})\]").expect("valid regex"));
static TSC_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(TS\d{3,5})\b").expect("valid regex"));

/// One session corpus the backfill walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Corpus {
    Prime,
    Claude,
    Opencode,
}

impl Corpus {
    /// Every corpus, in walk and emit order.
    pub const ALL: [Corpus; 3] = [Corpus::Prime, Corpus::Claude, Corpus::Opencode];

    /// The corpus name: the output subdirectory and the fingerprint scope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Corpus::Prime => "prime",
            Corpus::Claude => "claude",
            Corpus::Opencode => "opencode",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Corpus::ALL
            .into_iter()
            .find(|corpus| corpus.as_str() == name)
    }
}

/// Where a backfill reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillOptions {
    pub out_dir: PathBuf,
    pub prime_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub opencode_store: PathBuf,
    pub corpora: Vec<Corpus>,
}

/// Per (corpus, day): message counts and `(tool, token)` error counts.
#[derive(Debug, Default)]
struct DayAgg {
    user_msgs: u64,
    assistant_msgs: u64,
    errors: BTreeMap<(String, String), u64>,
}

type Bucket = BTreeMap<String, DayAgg>;

/// The structural error-class tokens in `text`, sorted, at most
/// [`MAX_TOKENS_PER_BLOB`]. No content.
#[must_use]
pub fn error_tokens(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = std::collections::BTreeSet::new();
    if text.contains("Traceback (most recent call last)") {
        out.insert("py:Traceback".to_string());
    }
    for found in EXCEPTION_CLASS.captures_iter(text) {
        out.insert(format!("exc:{}", &found[1]));
    }
    for found in RUSTC_CODE.captures_iter(text) {
        out.insert(format!("rustc:{}", &found[1]));
    }
    for found in TSC_CODE.captures_iter(text) {
        out.insert(format!("tsc:{}", &found[1]));
    }
    let lower = text.to_lowercase();
    for (needle, token) in [
        ("command not found", "sh:command-not-found"),
        ("no such file or directory", "sh:no-such-file"),
        ("permission denied", "sh:permission-denied"),
    ] {
        if lower.contains(needle) {
            out.insert(token.to_string());
        }
    }
    if text.starts_with("fatal:") || text.contains("\nfatal:") {
        out.insert("git:fatal".to_string());
    }
    if lower.contains("npm err!") {
        out.insert("npm:err".to_string());
    }
    out.into_iter().take(MAX_TOKENS_PER_BLOB).collect()
}

fn add_errors(agg: &mut DayAgg, tool: &str, blob: &str) {
    let tool = if tool.is_empty() { "tool" } else { tool };
    for token in error_tokens(blob) {
        *agg.errors.entry((tool.to_string(), token)).or_default() += 1;
    }
}

/// Python truthiness of a JSON value (`bool(value)`).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// A non-empty string field (`d.get(key) or <default>` over strings).
fn non_empty_str<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Epoch milliseconds of an ISO-8601 timestamp, as Python's
/// `datetime.fromisoformat` reads them after `Z` -> `+00:00`: a date, or a date
/// and time (`T` or space), optional seconds and fraction, optional `±HH:MM`
/// offset. A timestamp without an offset reads as UTC.
#[must_use]
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let text = text.replace('Z', "+00:00");
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = text.get(range)?;
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * 86_400_000;
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(millis);
    }
    let rest = rest.strip_prefix(['T', ' '])?;
    let (clock, offset) = match rest.find(['+', '-']) {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };
    let (clock, fraction) = match clock.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (clock, None),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let field = |part: &str, max: i64| -> Option<i64> {
        (part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<i64>().ok())
            .flatten()
            .filter(|value| *value <= max)
    };
    let (hour, minute, second) = match parts.as_slice() {
        [hour, minute] => (field(hour, 23)?, field(minute, 59)?, 0),
        [hour, minute, second] => (field(hour, 23)?, field(minute, 59)?, field(second, 59)?),
        _ => return None,
    };
    millis += ((hour * 60 + minute) * 60 + second) * 1_000;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || fraction.len() > 6 || parts.len() == 2 {
            return None;
        }
        if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let micros: i64 = format!("{fraction:0<6}").parse().ok()?;
        millis += micros / 1_000;
    }
    if let Some(offset) = offset {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let offset = &offset[1..];
        let (hours, minutes) = match offset.split_once(':') {
            Some((hours, minutes)) => (field(hours, 23)?, field(minutes, 59)?),
            None => (field(offset, 23)?, 0),
        };
        millis -= sign * (hours * 60 + minutes) * 60_000;
    }
    Some(millis)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since the epoch of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The UTC calendar day `YYYY-MM-DD` of epoch milliseconds.
fn day_key(millis: i64) -> String {
    pa_core::session::manager::format_iso(millis)[..10].to_string()
}

/// The day of an epoch-milliseconds field (Python `ms_to_dt`): a falsy or
/// non-numeric value has none.
fn ms_day(value: Option<&Value>) -> Option<String> {
    let millis = value
        .and_then(Value::as_f64)
        .filter(|millis| *millis != 0.0)?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "epoch milliseconds of a session timestamp fit in i64"
    )]
    let millis = millis.floor() as i64;
    Some(day_key(millis))
}

fn iso_day(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .and_then(parse_iso_ms)
        .map(day_key)
}

/// Every non-hidden `*.<extension>` file under `root`, recursively (Python's
/// `glob("**/*.<ext>", recursive=True)`: names starting with `.` are skipped;
/// symlinked directories are not followed).
fn walk_files(root: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk_files(&path, extension, out);
        } else if path.extension().is_some_and(|found| found == extension) {
            out.push(path);
        }
    }
}

/// The JSON objects of a JSONL file, one per non-blank line; lines that are
/// not JSON objects are skipped, invalid UTF-8 reads as replacement chars.
fn json_lines(path: &Path) -> Vec<Map<String, Value>> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if let Ok(Value::Object(object)) = serde_json::from_str(text) {
            out.push(object);
        }
    }
    out
}

fn json_file(path: &Path) -> Option<Map<String, Value>> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_str(&String::from_utf8_lossy(&bytes)) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

fn skipped(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIP_NAMES.contains(&name))
}

fn message_object(record: &Map<String, Value>) -> Option<&Map<String, Value>> {
    match record.get("message") {
        Some(Value::Object(message)) => Some(message),
        _ => None,
    }
}

fn walk_prime(bucket: &mut Bucket, prime_dir: &Path) {
    let mut files = Vec::new();
    walk_files(prime_dir, "jsonl", &mut files);
    for file in files.iter().filter(|file| !skipped(file)) {
        for record in json_lines(file) {
            if record.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(day) = iso_day(record.get("timestamp")) else {
                continue;
            };
            let agg = bucket.entry(day).or_default();
            let empty = Map::new();
            let message = message_object(&record).unwrap_or(&empty);
            let role = message.get("role").and_then(Value::as_str);
            match role {
                Some("user") => agg.user_msgs += 1,
                Some("assistant") => agg.assistant_msgs += 1,
                _ => {}
            }
            let Some(Value::Array(content)) = message.get("content") else {
                continue;
            };
            if role != Some("toolResult") {
                continue;
            }
            for part in content.iter().filter_map(Value::as_object) {
                if !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("toolResult" | "text")
                ) {
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    add_errors(agg, non_empty_str(part, "name").unwrap_or("tool"), text);
                }
            }
        }
    }
}

fn walk_claude(bucket: &mut Bucket, claude_dir: &Path) {
    let mut files = Vec::new();
    walk_files(claude_dir, "jsonl", &mut files);
    for file in files.iter().filter(|file| !skipped(file)) {
        for record in json_lines(file) {
            let Some(day) = iso_day(record.get("timestamp")) else {
                continue;
            };
            if !matches!(
                record.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            ) {
                continue;
            }
            let agg = bucket.entry(day).or_default();
            let empty = Map::new();
            let message = message_object(&record).unwrap_or(&empty);
            let role = message.get("role").and_then(Value::as_str);
            // Agent-to-agent turns do not inflate the denominator.
            let is_side = truthy(record.get("isSidechain"));
            let is_meta = truthy(record.get("isMeta"));
            let mut has_text = false;
            let mut has_tool_result = false;
            match message.get("content") {
                Some(Value::Array(content)) => {
                    for part in content.iter().filter_map(Value::as_object) {
                        match part.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                has_tool_result = true;
                                if truthy(part.get("is_error")) {
                                    *agg.errors
                                        .entry(("tool".into(), "tool_result:is_error".into()))
                                        .or_default() += 1;
                                }
                                let blob = match part.get("content") {
                                    Some(Value::String(text)) => Some(text.clone()),
                                    Some(Value::Array(items)) => {
                                        let parts: Vec<&str> = items
                                            .iter()
                                            .filter_map(Value::as_object)
                                            .filter_map(|item| item.get("text")?.as_str())
                                            .collect();
                                        (!parts.is_empty()).then(|| parts.join("\n"))
                                    }
                                    _ => None,
                                };
                                if let Some(blob) = blob.filter(|blob| !blob.is_empty()) {
                                    add_errors(agg, "tool", &blob);
                                }
                            }
                            Some("text") => has_text = true,
                            _ => {}
                        }
                    }
                }
                Some(Value::String(_)) => has_text = true,
                _ => {}
            }
            if role == Some("assistant") && !is_side && !is_meta {
                agg.assistant_msgs += 1;
            } else if role == Some("user") && has_text && !has_tool_result && !is_meta && !is_side {
                agg.user_msgs += 1;
            }
        }
    }
}

/// The `time` object of an opencode record (a non-object reads as none).
fn time_field<'a>(record: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match record.get("time") {
        Some(Value::Object(time)) => time.get(key),
        _ => None,
    }
}

/// The entries of `dir` (hidden ones included, like `os.scandir`).
fn dir_entries(dir: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn json_entries(dir: &Path) -> impl Iterator<Item = Map<String, Value>> {
    dir_entries(dir)
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|entry| json_file(&entry.path()))
}

fn walk_opencode(bucket: &mut Bucket, store: &Path) {
    let session_root = store.join("session").join("global");
    if !session_root.is_dir() {
        return;
    }
    // Sessions: id -> (created day, is a subagent session).
    let mut sessions: HashMap<String, (Option<String>, bool)> = HashMap::new();
    for entry in dir_entries(&session_root) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let Some(session) = json_file(&entry.path()) else {
            continue;
        };
        let Some(id) = non_empty_str(&session, "id") else {
            continue;
        };
        let created = time_field(&session, "created")
            .filter(|value| truthy(Some(value)))
            .and_then(|value| ms_day(Some(value)));
        sessions.insert(id.to_string(), (created, truthy(session.get("parentID"))));
    }
    // Messages: message id -> day; user turns of subagent sessions are skipped.
    let mut message_day: HashMap<String, String> = HashMap::new();
    for session_dir in dir_entries(&store.join("message")) {
        if !session_dir.path().is_dir() {
            continue;
        }
        let session = sessions.get(&*session_dir.file_name().to_string_lossy());
        let is_subagent = session.is_some_and(|(_, sub)| *sub);
        for message in json_entries(&session_dir.path()) {
            let day = match time_field(&message, "created").filter(|value| truthy(Some(value))) {
                Some(created) => ms_day(Some(created)),
                None => session.and_then(|(created, _)| created.clone()),
            };
            let Some(day) = day else {
                continue;
            };
            if let Some(id) = non_empty_str(&message, "id") {
                message_day.insert(id.to_string(), day.clone());
            }
            let agg = bucket.entry(day).or_default();
            match message.get("role").and_then(Value::as_str) {
                Some("user") if !is_subagent => agg.user_msgs += 1,
                Some("assistant") => agg.assistant_msgs += 1,
                _ => {}
            }
        }
    }
    // Parts: per-tool error tokens, on the day of their message.
    for message_dir in dir_entries(&store.join("part")) {
        if !message_dir.path().is_dir() {
            continue;
        }
        let day = message_day
            .get(&*message_dir.file_name().to_string_lossy())
            .cloned();
        for part in json_entries(&message_dir.path()) {
            let day = day.clone().or_else(|| {
                time_field(&part, "start")
                    .filter(|value| truthy(Some(value)))
                    .and_then(|value| ms_day(Some(value)))
            });
            let Some(day) = day else {
                continue;
            };
            if part.get("type").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let agg = bucket.entry(day).or_default();
            let tool = non_empty_str(&part, "tool").unwrap_or("?").to_string();
            if let Some(Value::Object(state)) = part.get("state") {
                if state.get("status").and_then(Value::as_str) == Some("error") {
                    *agg.errors
                        .entry((tool.clone(), "tool:error".into()))
                        .or_default() += 1;
                }
                if let Some(output) = state.get("output").and_then(Value::as_str) {
                    add_errors(agg, &tool, output);
                }
            }
        }
    }
}

/// The corpus-scoped fingerprint of a `(tool, token)`: never a runtime
/// ledger id (`sha1(corpus \0 tool \0 token)[:16]`).
#[must_use]
pub fn synthetic_fingerprint(corpus: &str, tool: &str, token: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha1::digest(format!("{corpus}\0{tool}\0{token}").as_bytes());
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Python `json.dump`'s default `ensure_ascii`: every non-ASCII char of the
/// serialized text escaped as `\uXXXX` (a surrogate pair above the BMP).
fn ensure_ascii(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

/// The serialized day file of one `(corpus, day)`, or `None` for a day with
/// neither turns nor errors.
fn day_record(corpus: Corpus, day: &str, agg: &DayAgg, sealed_at: &str) -> Option<String> {
    let fingerprints: Vec<Value> = agg
        .errors
        .iter()
        .map(|((tool, token), count)| {
            let mut entry = Map::new();
            entry.insert(
                "fingerprint".into(),
                Value::from(synthetic_fingerprint(corpus.as_str(), tool, token)),
            );
            entry.insert("name".into(), Value::from(token.as_str()));
            entry.insert("status".into(), Value::from("error"));
            entry.insert("failure".into(), Value::from(true));
            entry.insert("count".into(), Value::from(*count));
            entry.insert("message".into(), Value::from(token.as_str()));
            Value::Object(entry)
        })
        .collect();
    let turns = agg.user_msgs + agg.assistant_msgs;
    if turns == 0 && fingerprints.is_empty() {
        return None;
    }
    let mut record = Map::new();
    record.insert("schema".into(), Value::from(1));
    record.insert("day".into(), Value::from(day));
    record.insert("sealedAt".into(), Value::from(sealed_at));
    record.insert("turns".into(), Value::from(turns));
    record.insert("parseErrors".into(), Value::from(0));
    record.insert("sourceFiles".into(), Value::Array(Vec::new()));
    record.insert("commits".into(), Value::Array(Vec::new()));
    record.insert("fingerprints".into(), Value::Array(fingerprints));
    Some(format!(
        "{}\n",
        ensure_ascii(&stringify_pretty(&Value::Object(record)))
    ))
}

/// Walk the selected corpora and write their day files; `progress` gets each
/// step's line. Returns the number of day files written.
///
/// # Errors
///
/// Returns an error when an output directory or day file cannot be written.
pub fn run_backfill(
    options: &BackfillOptions,
    sealed_at: &str,
    progress: &mut dyn FnMut(&str),
) -> std::io::Result<usize> {
    let mut data: BTreeMap<Corpus, Bucket> = BTreeMap::new();
    for corpus in Corpus::ALL {
        if !options.corpora.contains(&corpus) {
            continue;
        }
        let bucket = data.entry(corpus).or_default();
        match corpus {
            Corpus::Prime => {
                progress("walking prime...");
                walk_prime(bucket, &options.prime_dir);
            }
            Corpus::Claude => {
                progress("walking claude...");
                walk_claude(bucket, &options.claude_dir);
            }
            Corpus::Opencode => {
                progress("walking opencode (this can be large; be patient)...");
                walk_opencode(bucket, &options.opencode_store);
            }
        }
    }
    crate::fs::create_private_dir(&options.out_dir)?;
    let mut written = 0;
    for (corpus, days) in &data {
        for (day, agg) in days {
            let Some(content) = day_record(*corpus, day, agg, sealed_at) else {
                continue;
            };
            let corpus_dir = options.out_dir.join(corpus.as_str());
            crate::fs::create_private_dir(&corpus_dir)?;
            let target = corpus_dir.join(format!("{day}.json"));
            let temp = corpus_dir.join(format!("{day}.json.{}.tmp", std::process::id()));
            if let Err(error) = crate::fs::write_private_file(&temp, content.as_bytes())
                .and_then(|()| std::fs::rename(&temp, &target))
            {
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
            written += 1;
        }
    }
    Ok(written)
}

/// The command line of `learning backfill`.
pub const BACKFILL_USAGE: &str = "learning backfill [--home <dir>] [--out <dir>] [--prime-dir <dir>] [--claude-dir <dir>] [--opencode-store <dir>] [--corpus prime|claude|opencode]...";

/// Parse `learning backfill`'s flags. Defaults: the output under the agent
/// dir's `learning/backfill` and prime sessions under its `sessions`, unless
/// `--home` names another home (then both derive from it, like the TS-era
/// script); the Claude Code and opencode roots under the home.
///
/// # Errors
///
/// Returns the message of an unknown flag, a missing value, an unknown
/// corpus, or an unresolvable home directory.
pub fn parse_backfill_args(args: &[String], agent_dir: &Path) -> Result<BackfillOptions, String> {
    let mut home: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut prime_dir: Option<PathBuf> = None;
    let mut claude_dir: Option<PathBuf> = None;
    let mut opencode_store: Option<PathBuf> = None;
    let mut corpora: Vec<Corpus> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--home" => home = Some(value("--home")?),
            "--out" => out = Some(value("--out")?),
            "--prime-dir" => prime_dir = Some(value("--prime-dir")?),
            "--claude-dir" => claude_dir = Some(value("--claude-dir")?),
            "--opencode-store" => opencode_store = Some(value("--opencode-store")?),
            "--corpus" => {
                let name = value("--corpus")?;
                let name = name.to_string_lossy();
                let corpus = Corpus::parse(&name).ok_or_else(|| {
                    format!("unknown corpus \"{name}\" (choose prime, claude or opencode)")
                })?;
                if !corpora.contains(&corpus) {
                    corpora.push(corpus);
                }
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    let explicit_home = home.is_some();
    let home = match home.or_else(pa_types::platform::dirs::home_dir) {
        Some(home) => home,
        None if claude_dir.is_some() && opencode_store.is_some() => PathBuf::new(),
        None => return Err("cannot resolve the home directory; pass --home".to_string()),
    };
    let prime_agent = home.join(".prime").join("agent");
    Ok(BackfillOptions {
        out_dir: out.unwrap_or_else(|| {
            if explicit_home {
                prime_agent.join("learning").join("backfill")
            } else {
                crate::store::trajectory_backfill_dir(agent_dir)
            }
        }),
        prime_dir: prime_dir.unwrap_or_else(|| {
            if explicit_home {
                prime_agent.join("sessions")
            } else {
                agent_dir.join("sessions")
            }
        }),
        claude_dir: claude_dir.unwrap_or_else(|| home.join(".claude").join("projects")),
        opencode_store: opencode_store.unwrap_or_else(|| {
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("storage")
        }),
        corpora: if corpora.is_empty() {
            Corpus::ALL.to_vec()
        } else {
            corpora
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_tokens_keep_structure_and_cap_at_eight() {
        let text = "Traceback (most recent call last):\nKeyError: 'x'\nerror[E0308]: mismatched\n\
                    src/a.ts: error TS2345\nbash: foo: command not found\nfatal: not a git repo\n\
                    npm ERR! code 1\nPermission denied\nValueError and ZeroDivisionError";
        assert_eq!(
            error_tokens(text),
            [
                "exc:KeyError",
                "exc:ValueError",
                "exc:ZeroDivisionError",
                "git:fatal",
                "npm:err",
                "py:Traceback",
                "rustc:E0308",
                "sh:command-not-found",
            ]
        );
        assert!(error_tokens("").is_empty());
    }

    #[test]
    fn iso_timestamps_read_like_python_fromisoformat() {
        let utc = |text: &str| parse_iso_ms(text).map(day_key);
        assert_eq!(
            utc("2026-10-01T23:30:00-02:00").as_deref(),
            Some("2026-10-02")
        );
        assert_eq!(
            utc("2026-10-02T00:30:00.5+02:00").as_deref(),
            Some("2026-10-01")
        );
        assert_eq!(utc("2026-10-02T12:00:00Z").as_deref(), Some("2026-10-02"));
        assert_eq!(utc("2026-10-02 12:00").as_deref(), Some("2026-10-02"));
        assert_eq!(utc("2026-10-02").as_deref(), Some("2026-10-02"));
        assert_eq!(
            parse_iso_ms("2026-10-02T12:00:00.123456Z"),
            parse_iso_ms("2026-10-02T12:00:00.123Z")
        );
        for invalid in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-02-30",
            "2026-10-02T25:00",
        ] {
            assert_eq!(parse_iso_ms(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn non_ascii_text_escapes_like_ensure_ascii() {
        assert_eq!(
            ensure_ascii("\"outil-é\" 😀"),
            "\"outil-\\u00e9\" \\ud83d\\ude00"
        );
    }
}
