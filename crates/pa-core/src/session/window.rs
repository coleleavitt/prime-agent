//! Generation-certified active session windows. Cold reads scan metadata to root;
//! warm reads touch only the canonical header and the retained transcript suffix.
//! JSONL remains authoritative; historical consumers explicitly hydrate.
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use pa_types::session::{FileEntry, SessionHeader};
use serde::{Deserialize, Serialize};

use super::window_cache::{self, Generation, Snapshot};
pub use super::window_cache::{AppendOwnership, append_cached, flush as flush_cache};
use super::{SessionContext, build_session_context};

const CHUNK_BYTES: usize = 64 * 1024;
/// Actual source read ranges and sidecar bytes for this open.
#[derive(Default, Clone, Debug)]
pub struct WindowReadStats {
    pub jsonl_bytes: u64,
    pub jsonl_ranges: Vec<(u64, u64)>,
    pub cache_bytes: u64,
    pub cache_hit: bool,
}

struct ReverseLines {
    file: std::fs::File,
    position: u64,
    pending: Vec<u8>,
    reads: WindowReadStats,
    line_start: u64,
}

impl ReverseLines {
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut pieces = Vec::new();
        loop {
            if let Some(index) = self.pending.iter().rposition(|byte| *byte == b'\n') {
                let mut line = self.pending.split_off(index + 1);
                self.pending.pop();
                self.line_start = self.position + self.pending.len() as u64 + 1;
                for piece in pieces.drain(..).rev() {
                    line.extend(piece);
                }
                if !line.is_empty() {
                    return Ok(Some(line));
                }
                continue;
            }
            pieces.push(std::mem::take(&mut self.pending));
            if self.position == 0 {
                self.line_start = 0;
                let line: Vec<u8> = pieces.into_iter().rev().flatten().collect();
                return Ok((!line.is_empty()).then_some(line));
            }
            let count = self.position.min(CHUNK_BYTES as u64) as usize;
            self.position -= count as u64;
            self.file.seek(SeekFrom::Start(self.position))?;
            self.pending.resize(count, 0);
            self.file.read_exact(&mut self.pending)?;
            self.reads.jsonl_bytes += count as u64;
            self.reads.jsonl_ranges.push((self.position, count as u64));
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    parent_id: Option<String>,
    message: Option<MessageMetadata>,
    custom_type: Option<String>,
}

#[derive(Deserialize)]
struct MessageMetadata {
    role: String,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<serde_json::Value>,
    content: Option<MessageContentMetadata>,
}

enum MessageContentMetadata {
    Blocks(Vec<ContentMetadata>),
    Text,
}

impl<'de> Deserialize<'de> for MessageContentMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> serde::de::Visitor<'de> for ContentVisitor {
            type Value = MessageContentMetadata;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("message text or content blocks")
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(MessageContentMetadata::Text)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element()? {
                    blocks.push(block);
                }
                Ok(MessageContentMetadata::Blocks(blocks))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

#[derive(Deserialize)]
struct ContentMetadata {
    #[serde(rename = "type")]
    kind: String,
}

/// Whole-branch accounting older than the retained suffix.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct WindowStats {
    pub total_messages: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_results: u64,
    pub tool_calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(with = "super::window_cache::float_bits")]
    pub cost: f64,
    /// The prefix's display-transcript rows by [`display_role`].
    pub display_rows: HashMap<String, usize>,
}

/// The role a session row shows under in the display transcript: a message row's own role,
/// `custom` for a `custom_message` row, `compactionSummary` for a compaction; `None` for
/// rows the transcript does not show.
#[must_use]
pub fn display_role<'a>(entry_type: &str, message_role: Option<&'a str>) -> Option<&'a str> {
    match entry_type {
        "message" => message_role,
        "custom_message" => Some("custom"),
        "compaction" => Some("compactionSummary"),
        _ => None,
    }
}

/// One older-path assistant row's spend-relevant usage: an attribution
/// targeting the row replaces it with the cumulative aggregate.
#[derive(Debug, Clone, Copy, Default)]
struct OlderPathUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

impl From<pa_types::ai::Usage> for OlderPathUsage {
    fn from(usage: pa_types::ai::Usage) -> Self {
        Self {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            cost: usage.cost.total.as_f64(),
        }
    }
}

impl OlderPathUsage {
    fn from_usage(usage: &serde_json::Value) -> Self {
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default()
        };
        Self {
            input: field("input"),
            output: field("output"),
            cache_read: field("cacheRead"),
            cache_write: field("cacheWrite"),
            cost: usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or_default(),
        }
    }
}

/// What one row of the newest-first walk asks of the open.
enum WalkStep {
    /// Keep walking toward the header.
    Next,
    /// The session header ended the walk (`None` when it does not parse).
    Header(Option<Box<FileEntry>>),
    /// The file needs the ordinary full reader.
    Reject,
}

/// Where the walk stands against the compaction boundary.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum WindowPhase {
    /// Retaining rows: the boundary is still ahead (toward the header).
    #[default]
    Retaining,
    /// The current row is the compaction's first kept row, the last retained one.
    Boundary,
    /// Past the boundary: the older rows only count.
    Done,
}

/// The newest-first walk from the leaf to the header: the retained window (the rows back to
/// the compaction boundary), the older path's accounting, and the settings the active
/// branch's newest rows pin.
#[derive(Default)]
struct WindowWalk {
    retained: Vec<FileEntry>,
    raw_entries: Vec<String>,
    metadata_entries: Vec<String>,
    message_count: usize,
    older_path_stats: WindowStats,
    older_usage: Vec<(String, OlderPathUsage)>,
    older_aggregates: HashMap<String, OlderPathUsage>,
    first_user_line: Option<Vec<u8>>,
    /// The next row id on the active branch (the last branch row's parent).
    expected: Option<String>,
    leaf_id: Option<String>,
    first_kept: Option<String>,
    retained_start: u64,
    compaction_count: usize,
    non_bootstrap: bool,
    goal: Option<crate::goals::GoalState>,
    /// The Anthropic subscription warning's once-per-session-lifecycle
    /// gate: set when the walk meets the marked-shown custom row (any
    /// position on the active branch hydrates it, exactly like `goal`).
    anthropic_warning_shown: bool,
    /// The newest plan-mode change on the branch (hydrated like `goal`).
    plan_mode: Option<bool>,
    phase: WindowPhase,
    seen: HashSet<String>,
    thinking: Option<String>,
    tier: Option<pa_types::ai::ServiceTier>,
    /// The branch pins a tier (`tier` `None`: pinned off).
    tier_present: bool,
    model: Option<(String, String)>,
    /// The boundary model: the usage fold seeds its timeline with this,
    /// not `model` (the leaf's).
    boundary_model: Option<(String, String)>,
}

impl WindowWalk {
    fn window_done(&self) -> bool {
        self.phase == WindowPhase::Done
    }

    /// One row, newest first.
    fn visit(&mut self, line: Vec<u8>, reader: &ReverseLines) -> io::Result<WalkStep> {
        let Ok(text) = std::str::from_utf8(&line) else {
            return Ok(WalkStep::Reject);
        };
        let Ok(meta) = serde_json::from_str::<Envelope>(text) else {
            return Ok(WalkStep::Reject);
        };
        if meta.kind == "session" {
            if reader.position != 0
                || reader
                    .pending
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
            {
                return Ok(WalkStep::Reject);
            }
            return Ok(WalkStep::Header(
                serde_json::from_slice::<FileEntry>(&line)
                    .ok()
                    .map(Box::new),
            ));
        }
        self.count_row(&meta, &line, text)?;
        let Some(id) = meta.id.as_deref() else {
            return Ok(WalkStep::Reject);
        };
        if !self.seen.insert(id.to_owned()) {
            return Ok(WalkStep::Reject);
        }
        if self.leaf_id.is_none() {
            self.leaf_id = Some(id.to_owned());
            self.expected.clone_from(&self.leaf_id);
        }
        let on_path = self.expected.as_deref() == Some(id);
        if on_path && !self.follow_path(&meta, id, text) {
            return Ok(WalkStep::Reject);
        }
        let entry = if self.parses_entry(on_path, &meta) {
            match serde_json::from_slice::<FileEntry>(&line) {
                Ok(entry) => Some(entry),
                Err(_) => return Ok(WalkStep::Reject),
            }
        } else {
            None
        };
        if on_path {
            self.note_path_entry(entry.as_ref(), id);
        }
        if self.phase != WindowPhase::Done {
            self.retained_start = reader.line_start;
            self.retained.push(entry.expect("window entries parsed"));
            self.raw_entries.push(
                String::from_utf8(line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
            if self.phase == WindowPhase::Boundary {
                self.phase = WindowPhase::Done;
            }
        }
        Ok(WalkStep::Next)
    }

    /// The counts every row contributes, on the active branch or not.
    fn count_row(&mut self, meta: &Envelope, line: &[u8], text: &str) -> io::Result<()> {
        if !matches!(
            meta.kind.as_str(),
            "model_change" | "thinking_level_change" | "service_tier_change"
        ) {
            self.non_bootstrap = true;
        }
        if meta.kind == "compaction" {
            self.compaction_count += 1;
        }
        if meta.kind == "message" {
            self.message_count += 1;
            if meta
                .message
                .as_ref()
                .is_some_and(|message| message.role == "user")
            {
                self.first_user_line = Some(line.to_vec());
            }
        } else if self.window_done()
            && (matches!(
                meta.kind.as_str(),
                "session_info"
                    | "session_state"
                    | "agent_status"
                    | "git_state"
                    | "child_usage_attributed"
            ) || matches!(
                (meta.kind.as_str(), meta.custom_type.as_deref()),
                (
                    "custom",
                    Some("provider_quota_park" | "provider_quota_resume")
                )
            ))
        {
            self.metadata_entries.push(
                String::from_utf8(line.to_vec())
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
        }
        if meta.kind == "child_usage_attributed" {
            // Older-path targets are dropped from the retained metadata, so
            // capture here or their attributed spend is lost. Newest-first,
            // the FIRST aggregate seen per target is the cumulative one.
            let value = serde_json::from_str::<serde_json::Value>(text).ok();
            let target = value
                .as_ref()
                .and_then(|value| value.get("targetId"))
                .and_then(serde_json::Value::as_str);
            // A malformed aggregate (null, a scalar) must not
            // replace a valid row usage with zeros.
            let aggregate = value
                .as_ref()
                .and_then(|value| value.get("aggregateUsage"))
                .filter(|aggregate| aggregate.is_object());
            if let (Some(target), Some(aggregate)) = (target, aggregate) {
                self.older_aggregates
                    .entry(target.to_owned())
                    .or_insert_with(|| OlderPathUsage::from_usage(aggregate));
            }
        }
        Ok(())
    }

    /// An active-branch row: the older path's accounting, the next expected id, and the
    /// newest assistant model. `false` when an assistant row names no model.
    fn follow_path(&mut self, meta: &Envelope, id: &str, text: &str) -> bool {
        if self.window_done() {
            self.count_older_path_row(meta, id, text);
        }
        self.expected.clone_from(&meta.parent_id);
        if let Some(message) = meta
            .message
            .as_ref()
            .filter(|message| message.role == "assistant" && self.model.is_none())
        {
            let (Some(provider), Some(name)) = (&message.provider, &message.model) else {
                return false;
            };
            self.model = Some((provider.clone(), name.clone()));
        }
        true
    }

    /// An active-branch row past the compaction boundary: its display row, message counts,
    /// and spend.
    fn count_older_path_row(&mut self, meta: &Envelope, id: &str, text: &str) {
        let stats = &mut self.older_path_stats;
        let role = display_role(
            &meta.kind,
            meta.message.as_ref().map(|message| message.role.as_str()),
        );
        if let Some(role) = role {
            *stats.display_rows.entry(role.to_owned()).or_default() += 1;
        }
        if meta.kind != "message" {
            // The older path's compactions and other billed calls.
            if let Some(row) = super::billed_row::billed_line(&meta.kind, text) {
                self.older_usage
                    .push((id.to_owned(), OlderPathUsage::from(row.usage)));
            }
            return;
        }
        let Some(message) = &meta.message else {
            return;
        };
        stats.total_messages += 1;
        match message.role.as_str() {
            "user" => stats.user_messages += 1,
            "toolResult" => stats.tool_results += 1,
            "assistant" => {
                stats.assistant_messages += 1;
                if let Some(MessageContentMetadata::Blocks(blocks)) = &message.content {
                    stats.tool_calls += blocks
                        .iter()
                        .filter(|block| block.kind == "toolCall")
                        .count() as u64;
                }
                // Recorded per row, not summed inline: an attribution
                // replaces its usage with the cumulative aggregate.
                self.older_usage.push((
                    id.to_owned(),
                    message
                        .usage
                        .as_ref()
                        .map(OlderPathUsage::from_usage)
                        .unwrap_or_default(),
                ));
            }
            _ => {}
        }
    }

    /// Whether the row's body parses: every retained row, and the sparse setting records
    /// the older active branch still pins; serde ignores the rest without allocating it.
    fn parses_entry(&self, on_path: bool, meta: &Envelope) -> bool {
        if !self.window_done() {
            return true;
        }
        let custom_type = meta.custom_type.as_deref();
        on_path
            && (matches!(
                meta.kind.as_str(),
                "model_change" | "thinking_level_change" | "service_tier_change"
            ) || (self.goal.is_none()
                && custom_type == Some(crate::goals::GOAL_STATE_CUSTOM_TYPE))
                || (!self.anthropic_warning_shown
                    && custom_type == Some(crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE))
                || (self.plan_mode.is_none()
                    && custom_type
                        == Some(crate::session_engine::plan_mode::PLAN_MODE_CHANGE_CUSTOM_TYPE)))
    }

    /// The settings the newest active-branch rows pin, and the compaction boundary.
    fn note_path_entry(&mut self, entry: Option<&FileEntry>, id: &str) {
        if self.goal.is_none() {
            self.goal = entry.and_then(valid_goal);
        }
        if !self.anthropic_warning_shown {
            self.anthropic_warning_shown = entry.is_some_and(valid_warning_shown);
        }
        if self.plan_mode.is_none() {
            self.plan_mode = entry.and_then(crate::session_engine::plan_mode::plan_mode_of_entry);
        }
        match entry {
            Some(FileEntry::ThinkingLevelChange { payload, .. }) if self.thinking.is_none() => {
                self.thinking = Some(payload.thinking_level.clone());
            }
            Some(FileEntry::ServiceTierChange { payload, .. }) if !self.tier_present => {
                self.tier = payload.service_tier;
                self.tier_present = true;
            }
            Some(FileEntry::ModelChange { payload, .. }) => {
                if self.model.is_none() {
                    self.model = Some((payload.provider.clone(), payload.model_id.clone()));
                }
                // The boundary model: the newest `model_change` in the discarded prefix.
                if self.window_done() && self.boundary_model.is_none() {
                    self.boundary_model =
                        Some((payload.provider.clone(), payload.model_id.clone()));
                }
            }
            Some(FileEntry::Compaction { payload, .. }) if self.first_kept.is_none() => {
                self.first_kept = Some(payload.first_kept_entry_id.clone());
            }
            _ => {}
        }
        if self.phase == WindowPhase::Retaining && self.first_kept.as_deref() == Some(id) {
            self.phase = WindowPhase::Boundary;
        }
    }

    /// The walked store: the window oldest-first under its header, the older path's spend
    /// folded, and the snapshot a warm open reuses.
    fn finish(
        mut self,
        path: &Path,
        header: FileEntry,
        generation: &Generation,
        reader: ReverseLines,
    ) -> io::Result<Option<WindowedSessionStore>> {
        let retained_whole_file = !self.window_done();
        self.retained.push(header);
        self.retained.reverse();
        self.raw_entries.reverse();
        self.metadata_entries.reverse();
        let metadata_entries =
            latest_metadata(std::mem::take(&mut self.metadata_entries), &self.retained)?;
        // Fold oldest-first (the cost sum's float order matches the pre-fold
        // walk): an attributed assistant reports its cumulative aggregate.
        let mut older_path_stats = self.older_path_stats;
        for (id, usage) in self.older_usage.iter().rev() {
            let folded = self.older_aggregates.get(id).unwrap_or(usage);
            older_path_stats.input += folded.input;
            older_path_stats.output += folded.output;
            older_path_stats.cache_read += folded.cache_read;
            older_path_stats.cache_write += folded.cache_write;
            older_path_stats.cost += folded.cost;
        }
        let first_user_message = self
            .first_user_line
            .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
            .and_then(|row| row.get("message").cloned());
        let Some(leaf_id) = self.leaf_id else {
            return Ok(None);
        };
        let mut retained = self.retained;
        let snapshot = Snapshot {
            version: window_cache::SNAPSHOT_VERSION,
            generation: generation.clone(),
            header: serde_json::to_string(&retained[0])?,
            start: self.retained_start,
            leaf: leaf_id.clone(),
            thinking: self.thinking.clone().unwrap_or_else(|| "off".to_owned()),
            thinking_present: self.thinking.is_some(),
            tier: self.tier,
            tier_present: self.tier_present,
            model: self.model.clone(),
            boundary_model: self.boundary_model.clone(),
            metadata: metadata_entries.clone(),
            message_count: self.message_count,
            compaction_count: self.compaction_count,
            stats: older_path_stats.clone(),
            first_user: first_user_message.clone(),
            goal: self.goal,
            anthropic_warning_shown: self.anthropic_warning_shown,
            plan_mode: self.plan_mode,
            non_bootstrap: self.non_bootstrap,
            retained_whole_file,
        };
        if !generation.valid(&reader.file, path)? {
            return Ok(None);
        }
        // A disposable sidecar failure must never prevent opening the source.
        let _ = window_cache::save(path, &snapshot);
        apply_attributions(&mut retained, &metadata_entries);
        super::apply_child_usage_attributions(&mut retained);
        Ok(Some(WindowedSessionStore {
            path: path.to_owned(),
            entries: retained,
            raw_entries: self.raw_entries,
            metadata_entries,
            message_count: self.message_count,
            older_path_stats,
            first_user_message,
            leaf_id,
            settings: SessionContext {
                messages: Vec::new(),
                thinking_level: self.thinking.unwrap_or_else(|| "off".to_owned()),
                service_tier: self.tier,
                model: self.model,
            },
            boundary_model: self.boundary_model,
            full: false,
            retained_whole_file,
            snapshot,
            reads: reader.reads,
            detached: false,
        }))
    }
}

/// The older-path metadata rows a window keeps (oldest first): the latest session info,
/// state and git rows, the latest attribution per retained target, and every quota
/// park/resume row.
fn latest_metadata(
    metadata_entries: Vec<String>,
    retained: &[FileEntry],
) -> io::Result<Vec<String>> {
    let retained_ids: HashSet<&str> = retained.iter().filter_map(FileEntry::id).collect();
    let mut latest = std::collections::HashMap::new();
    let mut keep = HashSet::new();
    for (index, row) in metadata_entries.iter().enumerate() {
        let value: serde_json::Value = serde_json::from_str(row)?;
        let kind = value["type"].as_str().unwrap_or("");
        match kind {
            "session_info" | "session_state" | "git_state" => {
                latest.insert(kind.to_owned(), index);
            }
            "child_usage_attributed"
                if value["targetId"]
                    .as_str()
                    .is_some_and(|id| retained_ids.contains(id)) =>
            {
                latest.insert(format!("attribution:{}", value["targetId"]), index);
            }
            // Quota-park episodes end on resume rows, so every
            // park/resume row must survive the keep filter.
            "custom"
                if matches!(
                    value["customType"].as_str(),
                    Some("provider_quota_park" | "provider_quota_resume")
                ) =>
            {
                keep.insert(index);
            }
            _ => {}
        }
    }
    keep.extend(latest.into_values());
    Ok(metadata_entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, row)| keep.contains(&index).then_some(row))
        .collect())
}

/// An active compacted window plus verified settings from its older ancestry.
/// The underlying JSONL is never rewritten by this reader.
pub struct WindowedSessionStore {
    path: PathBuf,
    entries: Vec<FileEntry>,
    raw_entries: Vec<String>,
    metadata_entries: Vec<String>,
    message_count: usize,
    older_path_stats: WindowStats,
    first_user_message: Option<serde_json::Value>,
    leaf_id: String,
    settings: SessionContext,
    boundary_model: Option<(String, String)>,
    full: bool,
    retained_whole_file: bool,
    snapshot: Snapshot,
    reads: WindowReadStats,
    /// The retained rows were handed to the manager ([`Self::take_retained`]);
    /// a second resident copy measured ~29.5MiB on the 10MiB fixture.
    detached: bool,
}

impl WindowedSessionStore {
    /// Return `None` for old schemas, torn rows, or ambiguous ancestry so callers
    /// can use their ordinary full reader. No size or message-count admission cap.
    ///
    /// # Errors
    ///
    /// I/O error when the file cannot be opened or read; other failures yield `Ok(None)`.
    ///
    /// # Panics
    ///
    /// The two internal `expect`s cannot fire (rows parse first; the header is proven).
    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        // The generation certificate anchors on unix inode identity; a
        // same-length replace is indistinguishable under weak non-unix metadata.
        if !cfg!(unix) {
            return Ok(None);
        }
        let mut file = std::fs::File::open(path)?;
        let generation = Generation::of(&file.metadata()?);
        let size = file.metadata()?.len();
        let mut reads = WindowReadStats::default();
        if let Some(snapshot) = window_cache::load(path, &file, &mut reads)
            && let Some(store) = Self::from_snapshot(path, &mut file, snapshot, reads.clone())?
        {
            return Ok(Some(store));
        }
        if size == 0 {
            return Ok(None);
        }
        // Appending to an unterminated row would merge two JSON records.
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Ok(None);
        }
        reads.jsonl_bytes += 1;
        reads.jsonl_ranges.push((size - 1, 1));
        let mut reader = ReverseLines {
            file,
            position: size,
            pending: Vec::new(),
            line_start: 0,
            reads,
        };
        let mut walk = WindowWalk::default();
        let header = loop {
            let Some(line) = reader.next()? else {
                break None;
            };
            match walk.visit(line, &reader)? {
                WalkStep::Next => {}
                WalkStep::Header(header) => break header,
                WalkStep::Reject => return Ok(None),
            }
        };
        let Some(
            header @ FileEntry::Header {
                header: SessionHeader {
                    version: Some(3), ..
                },
            },
        ) = header.map(|header| *header)
        else {
            return Ok(None);
        };
        if walk.expected.is_some() {
            return Ok(None);
        }
        walk.finish(path, header, &generation, reader)
    }

    fn from_snapshot(
        path: &Path,
        file: &mut std::fs::File,
        snapshot: Snapshot,
        mut reads: WindowReadStats,
    ) -> io::Result<Option<Self>> {
        file.seek(SeekFrom::Start(0))?;
        let mut header = Vec::new();
        let mut byte = [0];
        while file.read(&mut byte)? != 0 {
            header.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        reads.jsonl_bytes += header.len() as u64;
        reads.jsonl_ranges.push((0, header.len() as u64));
        let Ok(canonical) = serde_json::from_slice::<FileEntry>(&header) else {
            return Ok(None);
        };
        if serde_json::to_string(&canonical)? != snapshot.header
            || snapshot.start < header.len() as u64
            || snapshot.start > file.metadata()?.len()
        {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(snapshot.start))?;
        let mut suffix = String::new();
        file.read_to_string(&mut suffix)?;
        reads.jsonl_bytes += suffix.len() as u64;
        reads
            .jsonl_ranges
            .push((snapshot.start, suffix.len() as u64));
        let raw_entries: Vec<String> = suffix
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let mut entries = vec![canonical];
        for line in &raw_entries {
            let Ok(entry) = serde_json::from_str::<FileEntry>(line) else {
                return Ok(None);
            };
            entries.push(entry);
        }
        if entries.last().and_then(FileEntry::id) != Some(snapshot.leaf.as_str())
            || !snapshot.generation.valid(file, path)?
        {
            return Ok(None);
        }
        apply_attributions(&mut entries, &snapshot.metadata);
        super::apply_child_usage_attributions(&mut entries);
        reads.cache_hit = true;
        Ok(Some(Self {
            path: path.to_owned(),
            entries,
            raw_entries,
            metadata_entries: snapshot.metadata.clone(),
            message_count: snapshot.message_count,
            older_path_stats: snapshot.stats.clone(),
            first_user_message: snapshot.first_user.clone(),
            leaf_id: snapshot.leaf.clone(),
            settings: SessionContext {
                messages: Vec::new(),
                thinking_level: snapshot.thinking.clone(),
                service_tier: snapshot.tier,
                model: snapshot.model.clone(),
            },
            boundary_model: snapshot.boundary_model.clone(),
            full: false,
            retained_whole_file: snapshot.retained_whole_file,
            snapshot,
            reads,
            detached: false,
        }))
    }
    /// Move the retained raw JSONL rows out; holding both the raw lines and
    /// the parsed store doubles the load's resident peak.
    pub fn take_raw_entries(&mut self) -> Vec<String> {
        std::mem::take(&mut self.raw_entries)
    }

    /// Move the retained metadata rows out (same single-consumer contract).
    pub fn take_metadata_entries(&mut self) -> Vec<String> {
        std::mem::take(&mut self.metadata_entries)
    }

    /// Move the retained typed rows AND raw JSONL lines out for a one-copy
    /// adoption ([`super::manager::SessionManager::adopt_window`]): the
    /// manager owns the parsed trees; a detached window's context comes from it.
    pub fn take_retained(&mut self) -> (Vec<FileEntry>, Vec<String>) {
        self.detached = true;
        (
            std::mem::take(&mut self.entries),
            std::mem::take(&mut self.raw_entries),
        )
    }

    #[must_use]
    pub fn retained_detached(&self) -> bool {
        self.detached
    }

    #[must_use]
    pub fn full_history(&self) -> bool {
        self.full
    }

    /// Whether the walk retained every file row (no compaction boundary):
    /// the manager's entries already cover every persisted row.
    #[must_use]
    pub fn retained_whole_file(&self) -> bool {
        self.retained_whole_file
    }

    /// The walk-resolved settings (the owning manager's context overlay
    /// source while this window is attached).
    #[must_use]
    pub fn settings(&self) -> &SessionContext {
        &self.settings
    }

    #[must_use]
    pub fn has_non_bootstrap_entries(&self) -> bool {
        self.snapshot.non_bootstrap
    }
    #[must_use]
    pub fn read_stats(&self) -> &WindowReadStats {
        &self.reads
    }
    #[must_use]
    pub fn compaction_count(&self) -> usize {
        self.snapshot.compaction_count
    }
    #[must_use]
    pub fn goal_state(&self) -> Option<&crate::goals::GoalState> {
        self.snapshot.goal.as_ref()
    }
    /// Whether the active branch already carries the Anthropic subscription
    /// warning's shown marker (the once-per-session-lifecycle gate's
    /// hydrated read; [`crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE`]).
    #[must_use]
    pub fn anthropic_warning_shown(&self) -> bool {
        self.snapshot.anthropic_warning_shown
    }
    /// The newest plan-mode change on the active branch.
    #[must_use]
    pub fn plan_mode(&self) -> Option<bool> {
        self.snapshot.plan_mode
    }
    #[must_use]
    pub fn has_thinking_level(&self) -> bool {
        self.snapshot.thinking_present
    }
    #[must_use]
    pub fn has_service_tier(&self) -> bool {
        self.snapshot.tier_present
    }
    /// Fold a newly persisted linear entry without hydrating historical bodies.
    pub fn append_entry(&mut self, entry: FileEntry) {
        if !self.detached
            && let FileEntry::ChildUsageAttributed { payload, .. } = &entry
        {
            // A live append must fold into the retained assistant copy too, or
            // `context()` serves stale usage until reopen; a detached window
            // holds no bodies — the manager's append folds its copy.
            for retained in self.entries.iter_mut().rev() {
                if retained.id() == Some(payload.target_id.as_str()) {
                    if let FileEntry::Message {
                        message: pa_types::session::AgentMessage::Assistant(assistant),
                        ..
                    } = retained
                    {
                        assistant.usage = payload.aggregate_usage;
                    }
                    break;
                }
            }
        }
        update_snapshot(&mut self.snapshot, &entry);
        self.settings
            .thinking_level
            .clone_from(&self.snapshot.thinking);
        self.settings.service_tier = self.snapshot.tier;
        self.settings.model.clone_from(&self.snapshot.model);
        if let Some(id) = entry.id() {
            id.clone_into(&mut self.leaf_id);
        }
        if !self.detached {
            self.entries.push(entry);
        }
    }

    /// Retained file-order entries, including the original header.
    #[must_use]
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Original retained JSONL records, excluding the header. Preserves wire
    /// fields and map order for consumers with a separate raw entry type.
    #[must_use]
    pub fn raw_entries(&self) -> &[String] {
        &self.raw_entries
    }

    /// Older non-message records, in file order, for persisted metadata
    /// consumers. These are not a complete ancestor chain.
    #[must_use]
    pub fn metadata_entries(&self) -> &[String] {
        &self.metadata_entries
    }

    #[must_use]
    pub fn older_path_stats(&self) -> &WindowStats {
        &self.older_path_stats
    }

    #[must_use]
    pub fn message_count(&self) -> usize {
        self.message_count
    }

    #[must_use]
    pub fn first_user_message(&self) -> Option<&serde_json::Value> {
        self.first_user_message.as_ref()
    }

    #[must_use]
    pub fn leaf_id(&self) -> &str {
        &self.leaf_id
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_full_history(&self) -> bool {
        self.full
    }

    /// The model in effect at the retained-window boundary (the newest
    /// `model_change` in the discarded prefix): the per-model usage fold
    /// seeds its timeline with it; `None` without `model_change` rows or full history.
    #[must_use]
    pub fn boundary_model(&self) -> Option<&(String, String)> {
        self.boundary_model.as_ref()
    }

    /// Model context with settings resolved across the entire active ancestry.
    ///
    /// # Panics
    ///
    /// Panics when detached ([`Self::take_retained`]); `ensure_full_history` re-arms them.
    #[must_use]
    pub fn context(&self) -> SessionContext {
        assert!(
            !self.detached,
            "a detached window's transcript context comes from the owning manager"
        );
        let mut context = build_session_context(&self.entries, Some(&self.leaf_id));
        if !self.full {
            context
                .thinking_level
                .clone_from(&self.settings.thinking_level);
            context.service_tier = self.settings.service_tier;
            context.model.clone_from(&self.settings.model);
        }
        context
    }

    /// Hydrate off the async executor. This store is read-only: disk appends
    /// are included, the selected leaf remains pinned, and mutable stores
    /// must merge their own pending rows.
    ///
    /// # Errors
    ///
    /// Error when reading the session file fails or the blocking read task fails to join.
    pub async fn ensure_full_history(&mut self) -> anyhow::Result<()> {
        if self.full {
            return Ok(());
        }
        let path = self.path.clone();
        let entries = tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(path).map(|text| super::parse_session_entries(&text))
        })
        .await??;
        self.entries = entries;
        self.full = true;
        // Re-hydrated bodies re-arm the retained copies (and with them
        // `context()`), like a fresh walk.
        self.detached = false;
        Ok(())
    }
}

/// Whether `entry` is the session's Anthropic subscription warning shown
/// marker (the once-per-lifecycle gate's persisted row,
/// [`crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE`] with
/// `data.shown == true`).
fn valid_warning_shown(entry: &FileEntry) -> bool {
    let FileEntry::Custom { payload, .. } = entry else {
        return false;
    };
    payload.custom_type == crate::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE
        && payload
            .data
            .as_ref()
            .and_then(|data| data.get("shown"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn valid_goal(entry: &FileEntry) -> Option<crate::goals::GoalState> {
    let FileEntry::Custom { payload, .. } = entry else {
        return None;
    };
    let data = payload.data.as_ref()?;
    if payload.custom_type != crate::goals::GOAL_STATE_CUSTOM_TYPE
        || !crate::goals::is_persisted_goal_state(data)
    {
        return None;
    }
    serde_json::from_value(data.clone())
        .ok()
        .map(crate::goals::normalize_goal_state)
}
fn apply_attributions(entries: &mut [FileEntry], metadata: &[String]) {
    for row in metadata {
        if let Ok(FileEntry::ChildUsageAttributed { payload, .. }) = serde_json::from_str(row) {
            for entry in entries.iter_mut() {
                if entry.id() == Some(payload.target_id.as_str())
                    && let FileEntry::Message {
                        message: pa_types::session::AgentMessage::Assistant(message),
                        ..
                    } = entry
                {
                    message.usage = payload.aggregate_usage;
                }
            }
        }
    }
}
pub(super) fn update_snapshot(snapshot: &mut Snapshot, entry: &FileEntry) {
    if !matches!(
        entry,
        FileEntry::Header { .. }
            | FileEntry::ModelChange { .. }
            | FileEntry::ThinkingLevelChange { .. }
            | FileEntry::ServiceTierChange { .. }
    ) {
        snapshot.non_bootstrap = true;
    }
    if let Some(goal) = valid_goal(entry) {
        snapshot.goal = Some(goal);
    }
    // A live append of the warning marker folds into the retained window's
    // snapshot the same way, so the sidecar the next reload serves stays
    // current without a re-walk.
    if valid_warning_shown(entry) {
        snapshot.anthropic_warning_shown = true;
    }
    if let Some(enabled) = crate::session_engine::plan_mode::plan_mode_of_entry(entry) {
        snapshot.plan_mode = Some(enabled);
    }
    match entry {
        FileEntry::ThinkingLevelChange { payload, .. } => {
            snapshot.thinking.clone_from(&payload.thinking_level);
            snapshot.thinking_present = true;
        }
        FileEntry::ServiceTierChange { payload, .. } => {
            snapshot.tier = payload.service_tier;
            snapshot.tier_present = true;
        }
        FileEntry::ModelChange { payload, .. } => {
            snapshot.model = Some((payload.provider.clone(), payload.model_id.clone()));
        }
        FileEntry::Message { message, .. } => {
            snapshot.message_count += 1;
            if let pa_types::session::AgentMessage::Assistant(message) = message {
                snapshot.model = Some((message.provider.clone(), message.model.clone()));
            }
            if snapshot.first_user.is_none()
                && matches!(message, pa_types::session::AgentMessage::User(_))
            {
                snapshot.first_user = serde_json::to_value(message).ok();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
