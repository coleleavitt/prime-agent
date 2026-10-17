//! Claude Code's transcript format (2.1.296): one JSON object per line in
//! `~/.claude/projects/<sanitized cwd>/<session uuid>.jsonl`.
//!
//! Conversation entries (`user`, `assistant`, `system`, `attachment`) carry
//! `uuid`, `parentUuid` (null at a root), `isSidechain`, `timestamp`, `cwd`,
//! `gitBranch`, `version` and, for the two message kinds, the API `message`.
//! A streamed assistant reply is written one content block per entry, all
//! sharing `message.id`. A compaction writes a `system` entry with subtype
//! `compact_boundary` (null `parentUuid`, `logicalParentUuid` naming the
//! entry before it, `compactMetadata.preTokens`) followed by the summary as
//! a `user` entry with `isCompactSummary`. `progress` entries sit in the
//! parent chain and are skipped through. Metadata entries carry no uuid:
//! `custom-title`, `ai-title`, `agent-name`, `tag`, `summary` (`leafUuid`),
//! `last-prompt`, `file-history-snapshot` and many more; every type this
//! reader does not use is ignored.
//!
//! The conversation is the parent chain from the newest leaf (Claude Code's
//! `loadFullLog`: the latest-timestamp user/assistant entry no other entry
//! names as its parent), walked back to the root.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

/// A conversation entry's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Assistant,
    System,
    Attachment,
}

/// One conversation entry.
#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is one of the transcript's independent entry flags, read as written"
)]
pub(crate) struct Entry {
    pub kind: Kind,
    pub uuid: String,
    pub parent: Option<String>,
    pub logical_parent: Option<String>,
    pub sidechain: bool,
    pub meta: bool,
    /// Shown in Claude Code's transcript view only, never sent to the model.
    pub transcript_only: bool,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub subtype: Option<String>,
    pub compact_summary: bool,
    pub pre_tokens: Option<u64>,
    pub api_error: bool,
    /// Claude Code's `messageCount` rule, decided at parse time (a listing
    /// read drops the bulky messages).
    pub counts: bool,
    /// The API message (`role`, `content`, `id`, `model`), or null.
    pub message: Value,
}

impl Entry {
    pub(crate) fn is_compact_boundary(&self) -> bool {
        self.kind == Kind::System && self.subtype.as_deref() == Some("compact_boundary")
    }

    pub(crate) fn message_id(&self) -> Option<&str> {
        self.message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    }

    pub(crate) fn content(&self) -> &Value {
        self.message.get("content").unwrap_or(&Value::Null)
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.message.get("model").and_then(Value::as_str)
    }

    /// Claude Code's `messageCount` rule: a user entry with prompt content
    /// (not meta), or an assistant entry with non-empty text.
    pub(crate) fn counts_as_message(&self) -> bool {
        self.counts
    }
}

fn message_counts(kind: Kind, meta: bool, message: &Value) -> bool {
    let content = message.get("content").unwrap_or(&Value::Null);
    match kind {
        Kind::User => {
            if meta {
                return false;
            }
            match content {
                Value::String(text) => !text.trim().is_empty(),
                Value::Array(blocks) => blocks
                    .iter()
                    .any(|block| matches!(block_type(block), Some("text" | "image" | "document"))),
                _ => false,
            }
        }
        Kind::Assistant => content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block_type(block) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
            })
        }),
        Kind::System | Kind::Attachment => false,
    }
}

/// How much of each message a read keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Every message, for a conversion.
    Full,
    /// A listing's read: assistant messages and tool-result rows (the bulk
    /// of a transcript) are dropped once their flags are read.
    Listing,
}

pub(crate) fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// One parsed transcript.
#[derive(Debug, Default)]
pub(crate) struct Transcript {
    /// Conversation entries in file order.
    pub entries: Vec<Entry>,
    by_uuid: HashMap<String, usize>,
    /// `progress` entry uuid -> its parent (the chain skips through them).
    progress: HashMap<String, Option<String>>,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
    pub agent_name: Option<String>,
    pub tag: Option<String>,
    /// The last `summary` entry's text.
    pub summary: Option<String>,
    /// The last `last-prompt` entry's text.
    pub last_prompt: Option<String>,
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn non_empty(value: &Value, key: &str) -> Option<String> {
    string(value, key).filter(|text| !text.trim().is_empty())
}

impl Transcript {
    /// Parse a transcript's text; malformed lines and unknown entry types
    /// are skipped.
    #[cfg(test)]
    pub(crate) fn parse(text: &str) -> Self {
        let mut transcript = Self::default();
        for line in text.lines() {
            transcript.push_line(line.as_bytes(), Detail::Full);
        }
        transcript
    }

    fn push_line(&mut self, line: &[u8], detail: Detail) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        self.push(value, detail);
    }

    /// Read and parse a transcript file, line by line.
    pub(crate) fn read(path: &Path, detail: Detail) -> std::io::Result<Self> {
        use std::io::BufRead;

        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut transcript = Self::default();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            transcript.push_line(&line, detail);
        }
        Ok(transcript)
    }

    fn push(&mut self, mut value: Value, detail: Detail) {
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return;
        };
        let kind = match kind {
            "user" => Kind::User,
            "assistant" => Kind::Assistant,
            "system" => Kind::System,
            "attachment" => Kind::Attachment,
            "progress" => {
                if let Some(uuid) = string(&value, "uuid") {
                    self.progress.insert(uuid, string(&value, "parentUuid"));
                }
                return;
            }
            "custom-title" => {
                self.custom_title = non_empty(&value, "customTitle").or(self.custom_title.take());
                return;
            }
            "ai-title" => {
                self.ai_title = non_empty(&value, "aiTitle").or(self.ai_title.take());
                return;
            }
            "agent-name" => {
                self.agent_name = non_empty(&value, "agentName").or(self.agent_name.take());
                return;
            }
            "tag" => {
                self.tag = string(&value, "tag").filter(|tag| !tag.is_empty());
                return;
            }
            "summary" => {
                self.summary = non_empty(&value, "summary").or(self.summary.take());
                return;
            }
            "last-prompt" => {
                self.last_prompt = non_empty(&value, "lastPrompt").or(self.last_prompt.take());
                return;
            }
            _ => return,
        };
        let Some(uuid) = string(&value, "uuid") else {
            return;
        };
        let mut message = value
            .get_mut("message")
            .map(Value::take)
            .unwrap_or_default();
        let flag = |key: &str| value.get(key).and_then(Value::as_bool) == Some(true);
        let meta = flag("isMeta");
        let counts = message_counts(kind, meta, &message);
        let bulky = match kind {
            Kind::Assistant => true,
            Kind::User => message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block_type(block) == Some("tool_result"))
                }),
            Kind::System | Kind::Attachment => false,
        };
        if detail == Detail::Listing && bulky {
            message = Value::Null;
        }
        let entry = Entry {
            kind,
            parent: string(&value, "parentUuid"),
            logical_parent: string(&value, "logicalParentUuid"),
            sidechain: flag("isSidechain"),
            meta,
            transcript_only: flag("isVisibleInTranscriptOnly"),
            timestamp: string(&value, "timestamp"),
            cwd: non_empty(&value, "cwd"),
            git_branch: non_empty(&value, "gitBranch"),
            version: non_empty(&value, "version"),
            subtype: string(&value, "subtype"),
            compact_summary: flag("isCompactSummary"),
            pre_tokens: value
                .get("compactMetadata")
                .and_then(|meta| meta.get("preTokens"))
                .and_then(Value::as_u64),
            api_error: flag("isApiErrorMessage"),
            counts,
            message,
            uuid,
        };
        // A rewritten entry (same uuid) replaces the earlier one in place.
        if let Some(&index) = self.by_uuid.get(&entry.uuid) {
            self.entries[index] = entry;
            return;
        }
        self.by_uuid.insert(entry.uuid.clone(), self.entries.len());
        self.entries.push(entry);
    }

    /// The parent a chain walk follows: progress entries are skipped
    /// through, a compact boundary continues at its logical parent.
    fn resolved_parent(&self, entry: &Entry) -> Option<usize> {
        let mut parent = entry.parent.clone().or_else(|| {
            entry
                .is_compact_boundary()
                .then(|| entry.logical_parent.clone())
                .flatten()
        });
        let mut hops = 0;
        while let Some(uuid) = parent {
            if let Some(&index) = self.by_uuid.get(&uuid) {
                return Some(index);
            }
            hops += 1;
            if hops > self.progress.len() {
                return None;
            }
            parent = self.progress.get(&uuid).cloned().flatten();
        }
        None
    }

    /// The newest leaf: the latest-timestamp main-chain user or assistant
    /// entry that no other main-chain entry names as its parent (ties go to
    /// the later line).
    pub(crate) fn leaf(&self) -> Option<&Entry> {
        let parents: HashSet<usize> = self
            .entries
            .iter()
            .filter(|entry| !entry.sidechain)
            .filter_map(|entry| {
                let mut parent = entry.parent.clone();
                // Only the direct parent counts (through progress entries);
                // a boundary's logical parent stays a leaf of its old branch.
                let mut hops = 0;
                while let Some(uuid) = parent {
                    if let Some(&index) = self.by_uuid.get(&uuid) {
                        return Some(index);
                    }
                    hops += 1;
                    if hops > self.progress.len() {
                        return None;
                    }
                    parent = self.progress.get(&uuid).cloned().flatten();
                }
                None
            })
            .collect();
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                !entry.sidechain
                    && matches!(entry.kind, Kind::User | Kind::Assistant)
                    && !parents.contains(index)
            })
            .max_by(|(a_index, a), (b_index, b)| {
                a.timestamp
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(b.timestamp.as_deref().unwrap_or_default())
                    .then(a_index.cmp(b_index))
            })
            .map(|(_, entry)| entry)
    }

    /// The conversation from the root to `leaf`, across compact boundaries
    /// (each boundary stays in the chain where it happened).
    pub(crate) fn chain(&self, leaf: &Entry) -> Vec<&Entry> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = self.by_uuid.get(&leaf.uuid).copied();
        while let Some(index) = current {
            if !seen.insert(index) {
                break;
            }
            let entry = &self.entries[index];
            chain.push(entry);
            current = self.resolved_parent(entry);
        }
        chain.reverse();
        chain
    }

    /// The main conversation (the newest leaf's chain); empty when the
    /// transcript holds no user or assistant entry.
    pub(crate) fn conversation(&self) -> Vec<&Entry> {
        self.leaf().map(|leaf| self.chain(leaf)).unwrap_or_default()
    }

    /// A sidechain transcript (a subagent's): its first conversation entry
    /// is a sidechain one. Claude Code's resume list skips these.
    pub(crate) fn is_sidechain(&self) -> bool {
        self.entries.first().is_some_and(|entry| entry.sidechain)
    }

    /// The working directory: the first entry that records one.
    pub(crate) fn cwd(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| entry.cwd.as_deref())
    }

    /// The branch the session last worked on.
    pub(crate) fn git_branch(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| entry.git_branch.as_deref())
    }

    /// The first and the last timestamp of the main conversation's entries.
    pub(crate) fn time_span(&self) -> Option<(&str, &str)> {
        let mut stamps = self
            .entries
            .iter()
            .filter(|entry| !entry.sidechain)
            .filter_map(|entry| entry.timestamp.as_deref());
        let first = stamps.next()?;
        let last = stamps.fold(first, |latest, stamp| latest.max(stamp));
        Some((first, last))
    }

    /// The session's title by Claude Code's resume-picker priority:
    /// agent name, custom title, AI title, then the last summary.
    pub(crate) fn title(&self) -> Option<&str> {
        self.agent_name
            .as_deref()
            .or(self.custom_title.as_deref())
            .or(self.ai_title.as_deref())
            .or(self.summary.as_deref())
    }

    /// The prompt that stands in for a missing title (Claude Code's
    /// `firstPrompt`): the last recorded prompt, else the first real one.
    pub(crate) fn prompt_title(&self) -> Option<String> {
        self.last_prompt
            .as_deref()
            .map(|prompt| clip(&prompt.replace('\n', " ")))
            .or_else(|| first_prompt(&self.entries))
    }
}

const PROMPT_TITLE_MAX_CHARS: usize = 200;

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= PROMPT_TITLE_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(PROMPT_TITLE_MAX_CHARS).collect();
    format!("{}\u{2026}", head.trim())
}

/// The value of the first `<tag>...</tag>` in `text`.
fn tag_value<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// Claude Code's first-prompt scan (`vVr`): the first main-chain user entry
/// that is not a tool result or meta; a slash command reads as `/name args`
/// (a bare command is only the fallback), a `!` command as `! cmd`, other
/// harness-tagged text and interrupt markers are skipped.
fn first_prompt(entries: &[Entry]) -> Option<String> {
    let mut command_fallback: Option<String> = None;
    for entry in entries {
        if entry.kind != Kind::User || entry.meta || entry.sidechain {
            continue;
        }
        let texts: Vec<&str> = match entry.content() {
            Value::String(text) => vec![text.as_str()],
            Value::Array(blocks) => {
                if blocks
                    .iter()
                    .any(|block| block_type(block) == Some("tool_result"))
                {
                    continue;
                }
                blocks
                    .iter()
                    .filter(|block| block_type(block) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect()
            }
            _ => continue,
        };
        for text in texts {
            let text = text.replace('\n', " ");
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(name) = tag_value(text, "command-name") {
                let args = tag_value(text, "command-args").unwrap_or_default().trim();
                if args.is_empty() {
                    command_fallback.get_or_insert_with(|| name.to_string());
                    continue;
                }
                return Some(format!("{name} {args}"));
            }
            if let Some(command) = tag_value(text, "bash-input") {
                return Some(format!("! {command}"));
            }
            if is_harness_text(text) {
                continue;
            }
            return Some(clip(text));
        }
    }
    command_fallback
}

/// Claude Code's `zNn`: text opening with a lowercase tag, or an
/// interrupt marker.
fn is_harness_text(text: &str) -> bool {
    let text = text.trim_start();
    if text.starts_with("[Request interrupted by user") {
        return true;
    }
    let mut chars = text.chars();
    chars.next() == Some('<') && chars.next().is_some_and(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[Value]) -> String {
        values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn user(uuid: &str, parent: Option<&str>, at: &str, content: Value) -> Value {
        let mut line = serde_json::json!({
            "type": "user", "uuid": uuid, "parentUuid": parent, "isSidechain": false,
            "timestamp": at, "cwd": "/w", "gitBranch": "main", "sessionId": "s",
            "message": { "role": "user" },
        });
        line["message"]["content"] = content;
        line
    }

    fn assistant(uuid: &str, parent: &str, at: &str, text: &str) -> Value {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "parentUuid": parent, "isSidechain": false,
            "timestamp": at, "cwd": "/w", "gitBranch": "feature",
            "message": { "id": format!("msg-{uuid}"), "role": "assistant", "model": "claude-x",
                         "content": [{ "type": "text", "text": text }] },
        })
    }

    #[test]
    fn the_newest_leaf_wins_and_its_chain_runs_to_the_root() {
        let text = lines(&[
            user("u1", None, "2026-01-01T00:00:01.000Z", Value::from("first")),
            assistant("a1", "u1", "2026-01-01T00:00:02.000Z", "reply"),
            // An abandoned branch from u1 (an earlier rewind).
            assistant("a0", "u1", "2026-01-01T00:00:01.500Z", "old"),
            user(
                "u2",
                Some("a1"),
                "2026-01-01T00:00:03.000Z",
                Value::from("second"),
            ),
            serde_json::json!({ "type": "file-history-snapshot", "messageId": "u2", "snapshot": {} }),
            serde_json::json!({ "type": "some-future-entry", "uuid": "zz", "parentUuid": "u2" }),
            serde_json::json!({ "type": "custom-title", "customTitle": "Named", "sessionId": "s" }),
        ]);
        let transcript = Transcript::parse(&format!("{text}\n{{\"type\":\"user\",broken\n\n"));
        let chain: Vec<&str> = transcript
            .conversation()
            .iter()
            .map(|entry| entry.uuid.as_str())
            .collect();
        assert_eq!(chain, ["u1", "a1", "u2"]);
        assert_eq!(transcript.custom_title.as_deref(), Some("Named"));
        assert_eq!(transcript.git_branch(), Some("main"));
        assert_eq!(transcript.cwd(), Some("/w"));
    }

    #[test]
    fn sidechain_entries_never_join_the_main_chain() {
        let mut side = assistant("s1", "u1", "2026-01-01T00:00:09.000Z", "subagent");
        side["isSidechain"] = Value::Bool(true);
        let text = lines(&[
            user("u1", None, "2026-01-01T00:00:01.000Z", Value::from("go")),
            side,
            assistant("a1", "u1", "2026-01-01T00:00:02.000Z", "done"),
        ]);
        let transcript = Transcript::parse(&text);
        let chain: Vec<&str> = transcript
            .conversation()
            .iter()
            .map(|entry| entry.uuid.as_str())
            .collect();
        assert_eq!(chain, ["u1", "a1"]);
        assert!(!transcript.is_sidechain());
        let mut first = user("x", None, "2026-01-01T00:00:01.000Z", Value::from("sub"));
        first["isSidechain"] = Value::Bool(true);
        assert!(Transcript::parse(&first.to_string()).is_sidechain());
    }

    #[test]
    fn a_compact_boundary_continues_at_its_logical_parent() {
        let text = lines(&[
            user(
                "u1",
                None,
                "2026-01-01T00:00:01.000Z",
                Value::from("old work"),
            ),
            assistant("a1", "u1", "2026-01-01T00:00:02.000Z", "old reply"),
            serde_json::json!({
                "type": "system", "subtype": "compact_boundary", "uuid": "b1",
                "parentUuid": null, "logicalParentUuid": "a1", "isSidechain": false,
                "timestamp": "2026-01-01T00:00:03.000Z", "content": "Conversation compacted",
                "compactMetadata": { "trigger": "manual", "preTokens": 1234 },
            }),
            {
                let mut summary = user(
                    "u2",
                    Some("b1"),
                    "2026-01-01T00:00:04.000Z",
                    Value::from("This session is being continued..."),
                );
                summary["isCompactSummary"] = Value::Bool(true);
                summary
            },
            assistant("a2", "u2", "2026-01-01T00:00:05.000Z", "new reply"),
        ]);
        let transcript = Transcript::parse(&text);
        let chain = transcript.conversation();
        let ids: Vec<&str> = chain.iter().map(|entry| entry.uuid.as_str()).collect();
        assert_eq!(ids, ["u1", "a1", "b1", "u2", "a2"]);
        assert!(chain[2].is_compact_boundary());
        assert_eq!(chain[2].pre_tokens, Some(1234));
        assert!(chain[3].compact_summary);
    }

    #[test]
    fn progress_entries_are_skipped_through() {
        let text = lines(&[
            user("u1", None, "2026-01-01T00:00:01.000Z", Value::from("go")),
            serde_json::json!({ "type": "progress", "uuid": "p1", "parentUuid": "u1" }),
            assistant("a1", "p1", "2026-01-01T00:00:02.000Z", "done"),
        ]);
        let transcript = Transcript::parse(&text);
        let ids: Vec<&str> = transcript
            .conversation()
            .iter()
            .map(|entry| entry.uuid.as_str())
            .collect();
        assert_eq!(ids, ["u1", "a1"]);
    }

    #[test]
    fn titles_follow_the_resume_pickers_priority() {
        let base = vec![user(
            "u1",
            None,
            "2026-01-01T00:00:01.000Z",
            Value::from("the prompt"),
        )];
        let with = |extra: &[Value]| {
            let mut all = base.clone();
            all.extend_from_slice(extra);
            Transcript::parse(&lines(&all))
        };
        let summary =
            serde_json::json!({ "type": "summary", "summary": "Summary", "leafUuid": "u1" });
        let ai = serde_json::json!({ "type": "ai-title", "aiTitle": "AI title", "sessionId": "s" });
        let custom = serde_json::json!({ "type": "custom-title", "customTitle": "Custom", "sessionId": "s" });
        let agent =
            serde_json::json!({ "type": "agent-name", "agentName": "Agent", "sessionId": "s" });
        assert_eq!(with(&[]).title(), None);
        assert_eq!(with(&[]).prompt_title().as_deref(), Some("the prompt"));
        assert_eq!(
            with(std::slice::from_ref(&summary)).title(),
            Some("Summary")
        );
        assert_eq!(
            with(&[summary.clone(), ai.clone()]).title(),
            Some("AI title")
        );
        assert_eq!(
            with(&[custom.clone(), summary.clone(), ai.clone()]).title(),
            Some("Custom")
        );
        assert_eq!(with(&[custom, summary, ai, agent]).title(), Some("Agent"));
        let last = serde_json::json!({ "type": "last-prompt", "lastPrompt": "latest\nask", "leafUuid": "u1" });
        assert_eq!(with(&[last]).prompt_title().as_deref(), Some("latest ask"));
    }

    #[test]
    fn the_first_prompt_skips_harness_rows_and_reads_commands() {
        let mut meta = user(
            "m",
            None,
            "2026-01-01T00:00:00.000Z",
            Value::from("Caveat: ..."),
        );
        meta["isMeta"] = Value::Bool(true);
        let entries = Transcript::parse(&lines(&[
            meta,
            user(
                "c",
                None,
                "2026-01-01T00:00:01.000Z",
                Value::from("<command-name>/clear</command-name><command-args></command-args>"),
            ),
            user(
                "t",
                None,
                "2026-01-01T00:00:02.000Z",
                serde_json::json!([{ "type": "tool_result", "tool_use_id": "x", "content": "out" }]),
            ),
            user(
                "r",
                None,
                "2026-01-01T00:00:03.000Z",
                Value::from("<local-command-stdout>hi</local-command-stdout>"),
            ),
            user(
                "p",
                None,
                "2026-01-01T00:00:04.000Z",
                serde_json::json!([{ "type": "text", "text": format!("fix {}", "x".repeat(300)) }]),
            ),
        ]));
        let prompt = first_prompt(&entries.entries).unwrap();
        assert!(prompt.starts_with("fix xxx"), "{prompt}");
        assert!(prompt.ends_with('\u{2026}'));
        assert_eq!(prompt.chars().count(), PROMPT_TITLE_MAX_CHARS + 1);
        let only_command = Transcript::parse(&lines(&[user(
            "c",
            None,
            "2026-01-01T00:00:01.000Z",
            Value::from("<command-name>/review</command-name><command-args>42</command-args>"),
        )]));
        assert_eq!(
            first_prompt(&only_command.entries).as_deref(),
            Some("/review 42")
        );
        let bash = Transcript::parse(&lines(&[user(
            "b",
            None,
            "2026-01-01T00:00:01.000Z",
            Value::from("<bash-input>ls</bash-input>"),
        )]));
        assert_eq!(first_prompt(&bash.entries).as_deref(), Some("! ls"));
    }

    #[test]
    fn message_counts_follow_claude_code() {
        let mut meta = user("m", None, "2026-01-01T00:00:00.000Z", Value::from("meta"));
        meta["isMeta"] = Value::Bool(true);
        let transcript = Transcript::parse(&lines(&[
            meta,
            user(
                "u",
                Some("m"),
                "2026-01-01T00:00:01.000Z",
                Value::from("hi"),
            ),
            user(
                "t",
                Some("u"),
                "2026-01-01T00:00:02.000Z",
                serde_json::json!([{ "type": "tool_result", "tool_use_id": "x", "content": "o" }]),
            ),
            assistant("a", "t", "2026-01-01T00:00:03.000Z", "  "),
            assistant("b", "a", "2026-01-01T00:00:04.000Z", "answer"),
        ]));
        let count = transcript
            .conversation()
            .iter()
            .filter(|entry| entry.counts_as_message())
            .count();
        assert_eq!(count, 2);
    }
}
