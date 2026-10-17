//! A Claude Code conversation as prime-agent session entries.
//!
//! - User prompts become `user` messages (text and images kept; meta rows,
//!   transcript-only rows and tool results are not prompts).
//! - An assistant reply (every entry sharing one `message.id`, siblings
//!   included) becomes one `assistant` message on the `anthropic` provider
//!   with Claude Code's model id, so a resume continues on the same model
//!   when it is available. Text is kept. Thinking is dropped: its signature
//!   covers a turn whose tool calls are rewritten here, so the provider
//!   would refuse it. Claude Code's tools do not exist in this harness (the
//!   model works through `python_repl`), so each tool call is rendered as a
//!   text record in the reply, and its result as a
//!   `claude_code_tool_result` custom message right after it (the model
//!   reads it as a user turn). Usage is zero: the spend was Claude Code's.
//! - A compact boundary and its summary become a `compaction` entry: the
//!   model sees the summary and what follows, the history before it stays
//!   visible in the transcript, like Claude Code.
//! - The session is named after the Claude Code title, and one closing
//!   `claude_code_import` note tells the model where the conversation came
//!   from.

use std::collections::{HashMap, HashSet};

use pa_core::session::timestamp_to_millis;
use pa_types::ai::{
    AssistantContentBlock,
    AssistantMessage,
    ImageContent,
    StopReason,
    TextContent,
    Usage,
    UserContent,
    UserContentBlock,
    UserMessage,
};
use pa_types::session::{
    AgentMessage,
    CompactionEntry,
    CustomMessageEntry,
    EntryBase,
    FileEntry,
    GitContext,
    SessionHeader,
    SessionInfoEntry,
};
use serde_json::{Map, Value, json};

use crate::transcript::{Entry, Kind, Transcript, block_type};

/// The custom type of a Claude Code tool result row.
pub const TOOL_RESULT_CUSTOM_TYPE: &str = "claude_code_tool_result";
/// The custom type of the closing import note.
pub const IMPORT_NOTE_CUSTOM_TYPE: &str = "claude_code_import";
/// The header key recording where an imported session came from.
pub const IMPORTED_FROM_KEY: &str = "importedFrom";

const TOOL_INPUT_MAX_CHARS: usize = 4_000;
const TOOL_RESULT_MAX_CHARS: usize = 20_000;
const MISSING_SUMMARY: &str =
    "(Claude Code compacted the conversation here; no summary was recorded.)";

/// The source a conversion records in the new header.
#[derive(Debug, Clone)]
pub(crate) struct Source<'a> {
    pub session_id: &'a str,
    pub path: &'a str,
}

/// A converted session: every line of the new file, header first.
#[derive(Debug)]
pub(crate) struct Converted {
    pub entries: Vec<FileEntry>,
    pub cwd: String,
    pub leaf_uuid: String,
}

fn text_block(text: String) -> TextContent {
    TextContent {
        text,
        text_signature: None,
        rest: Map::default(),
    }
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n[... {} more characters not imported]", count - max)
}

/// One tool call as the text record the reply carries.
fn render_tool_call(label: &str, name: &str, input: &Value) -> String {
    let detail = match (name, input.get("command").and_then(Value::as_str)) {
        ("Bash", Some(command)) => format!("$ {command}"),
        _ => serde_json::to_string(input).unwrap_or_default(),
    };
    format!(
        "[Claude Code {label}: {name}]\n{}",
        clip(&detail, TOOL_INPUT_MAX_CHARS)
    )
}

/// A tool result's content as text (images are named, not imported).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block_type(block) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Some("image") => "[image]".to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A user entry's prompt content (text, images, documents named); `None`
/// when nothing is left once tool results are taken out.
fn user_content(entry: &Entry) -> Option<UserContent> {
    match entry.content() {
        Value::String(text) if !text.trim().is_empty() => Some(UserContent::Text(text.clone())),
        Value::Array(blocks) => {
            let converted: Vec<UserContentBlock> = blocks
                .iter()
                .filter_map(|block| match block_type(block) {
                    Some("text") => block
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| UserContentBlock::Text(text_block(text.to_string()))),
                    Some("image") => {
                        let source = block.get("source")?;
                        if source.get("type").and_then(Value::as_str) != Some("base64") {
                            return Some(UserContentBlock::Text(text_block("[image]".into())));
                        }
                        Some(UserContentBlock::Image(ImageContent {
                            data: source.get("data")?.as_str()?.to_string(),
                            mime_type: source.get("media_type")?.as_str()?.to_string(),
                            rest: Map::default(),
                        }))
                    }
                    Some("document") => {
                        Some(UserContentBlock::Text(text_block("[document]".into())))
                    }
                    _ => None,
                })
                .collect();
            (!converted.is_empty()).then_some(UserContent::Blocks(converted))
        }
        _ => None,
    }
}

fn summary_text(entry: &Entry) -> String {
    match user_content(entry) {
        Some(content) => content.text(),
        None => String::new(),
    }
}

/// Every tool result in the transcript by `tool_use_id`: (text, is error).
fn tool_results(transcript: &Transcript) -> HashMap<String, (String, bool)> {
    let mut results = HashMap::new();
    for entry in &transcript.entries {
        if entry.kind != Kind::User || entry.sidechain {
            continue;
        }
        let Some(blocks) = entry.content().as_array() else {
            continue;
        };
        for block in blocks {
            if block_type(block) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let error = block.get("is_error").and_then(Value::as_bool) == Some(true);
            results.entry(id.to_string()).or_insert_with(|| {
                (
                    result_text(block.get("content").unwrap_or(&Value::Null)),
                    error,
                )
            });
        }
    }
    results
}

/// Every main-chain entry of each assistant `message.id`, in file order (a
/// parallel tool call's blocks can sit on sibling branches).
fn reply_groups(transcript: &Transcript) -> HashMap<&str, Vec<&Entry>> {
    let mut groups: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for entry in &transcript.entries {
        if entry.kind == Kind::Assistant && !entry.sidechain {
            if let Some(id) = entry.message_id() {
                groups.entry(id).or_default().push(entry);
            }
        }
    }
    groups
}

/// The entry builder: linear ids and parent links.
struct Builder {
    entries: Vec<FileEntry>,
    last_id: Option<String>,
    fallback_timestamp: String,
}

impl Builder {
    fn base(&mut self, timestamp: Option<&str>) -> EntryBase {
        let id = format!("{:08x}", self.entries.len());
        EntryBase {
            id: Some(id.clone()),
            parent_id: self.last_id.replace(id),
            timestamp: Some(timestamp.unwrap_or(&self.fallback_timestamp).to_string()),
            rest: Map::default(),
        }
    }

    fn push_message(&mut self, timestamp: Option<&str>, message: AgentMessage) {
        let base = self.base(timestamp);
        self.entries.push(FileEntry::Message { message, base });
    }

    fn push_custom(&mut self, timestamp: Option<&str>, custom_type: &str, text: String) {
        let base = self.base(timestamp);
        self.entries.push(FileEntry::CustomMessage {
            payload: CustomMessageEntry {
                custom_type: custom_type.to_string(),
                content: UserContent::Text(text),
                details: None,
                display: true,
                rest: Map::default(),
            },
            base,
        });
    }

    fn push_compaction(&mut self, timestamp: Option<&str>, summary: String, tokens_before: u64) {
        let base = self.base(timestamp);
        // The compaction keeps nothing before it in the model's view: it
        // names itself as the first kept entry, which no earlier entry is.
        let first_kept_entry_id = base.id.clone().unwrap_or_default();
        self.entries.push(FileEntry::Compaction {
            payload: CompactionEntry {
                summary,
                first_kept_entry_id,
                tokens_before,
                details: None,
                from_hook: None,
                custom_instructions: None,
                usage: None,
                harness_digest: None,
                harness_state_fingerprint: None,
            },
            base,
        });
    }
}

fn millis(timestamp: Option<&str>) -> u64 {
    timestamp.map(timestamp_to_millis).unwrap_or_default()
}

fn assistant_message(
    model: &str,
    content: Vec<AssistantContentBlock>,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: model.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp,
        rest: Map::default(),
        discarded_usage: None,
    })
}

/// One reply's blocks: its text and tool-call records, plus the tool calls
/// (id, name) whose results follow it.
fn reply_blocks(group: &[&Entry]) -> (Vec<AssistantContentBlock>, Vec<(String, String)>) {
    let mut content = Vec::new();
    let mut calls = Vec::new();
    for entry in group {
        let Some(blocks) = entry.content().as_array() else {
            continue;
        };
        for block in blocks {
            let text = match block_type(block) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_string),
                Some(kind @ ("tool_use" | "server_tool_use")) => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        calls.push((id.to_string(), name.to_string()));
                    }
                    let label = if kind == "tool_use" {
                        "tool call"
                    } else {
                        "server tool call"
                    };
                    Some(render_tool_call(
                        label,
                        name,
                        block.get("input").unwrap_or(&Value::Null),
                    ))
                }
                Some(kind) if kind.ends_with("_tool_result") => Some(format!(
                    "[Claude Code server tool result: {kind}]\n{}",
                    clip(
                        &result_text(block.get("content").unwrap_or(&Value::Null)),
                        TOOL_INPUT_MAX_CHARS,
                    )
                )),
                // thinking / redacted_thinking and unknown blocks: dropped.
                _ => None,
            };
            if let Some(text) = text {
                content.push(AssistantContentBlock::Text(text_block(text)));
            }
        }
    }
    (content, calls)
}

/// The conversation walk: the builder plus what it reads across entries.
struct Walk<'t> {
    builder: Builder,
    results: HashMap<String, (String, bool)>,
    groups: HashMap<&'t str, Vec<&'t Entry>>,
    /// A compact boundary waiting for its summary: (timestamp, tokens before).
    pending_boundary: Option<(Option<&'t str>, u64)>,
    emitted_replies: HashSet<&'t str>,
}

impl<'t> Walk<'t> {
    /// Close a boundary whose summary never came.
    fn flush_boundary(&mut self) {
        if let Some((at, tokens)) = self.pending_boundary.take() {
            self.builder
                .push_compaction(at, MISSING_SUMMARY.to_string(), tokens);
        }
    }

    fn entry(&mut self, entry: &'t Entry) {
        let timestamp = entry.timestamp.as_deref();
        if entry.is_compact_boundary() {
            self.flush_boundary();
            self.pending_boundary = Some((timestamp, entry.pre_tokens.unwrap_or_default()));
            return;
        }
        if entry.kind == Kind::User && entry.compact_summary {
            let (at, tokens) = self.pending_boundary.take().unwrap_or((timestamp, 0));
            let summary = summary_text(entry);
            let summary = if summary.trim().is_empty() {
                MISSING_SUMMARY.to_string()
            } else {
                summary
            };
            self.builder.push_compaction(at, summary, tokens);
            return;
        }
        self.flush_boundary();
        match entry.kind {
            Kind::User => {
                if entry.meta || entry.transcript_only {
                    return;
                }
                if let Some(content) = user_content(entry) {
                    self.builder.push_message(
                        timestamp,
                        AgentMessage::User(UserMessage {
                            content,
                            timestamp: millis(timestamp),
                            rest: Map::default(),
                        }),
                    );
                }
            }
            Kind::Assistant => self.reply(entry),
            Kind::System | Kind::Attachment => {}
        }
    }

    /// One assistant reply (every entry of its `message.id`, once) and the
    /// results of its tool calls.
    fn reply(&mut self, entry: &'t Entry) {
        if entry.api_error || entry.model() == Some("<synthetic>") {
            return;
        }
        let single = [entry];
        let group: &[&Entry] = match entry.message_id() {
            Some(id) => {
                if !self.emitted_replies.insert(id) {
                    return;
                }
                self.groups.get(id).map_or(&single[..], Vec::as_slice)
            }
            None => &single,
        };
        let (content, calls) = reply_blocks(group);
        if content.is_empty() {
            return;
        }
        let timestamp = entry.timestamp.as_deref();
        let model = entry.model().unwrap_or("unknown");
        let at = group.last().and_then(|last| last.timestamp.as_deref());
        let rendered: Vec<String> = calls
            .iter()
            .filter_map(|(id, name)| {
                let (text, error) = self.results.get(id)?;
                let label = if *error { "tool error" } else { "tool result" };
                Some(format!(
                    "[Claude Code {label}: {name}]\n{}",
                    clip(text, TOOL_RESULT_MAX_CHARS)
                ))
            })
            .collect();
        self.builder.push_message(
            timestamp,
            assistant_message(model, content, millis(timestamp)),
        );
        if !rendered.is_empty() {
            self.builder
                .push_custom(at, TOOL_RESULT_CUSTOM_TYPE, rendered.join("\n\n"));
        }
    }
}

/// The new session's header: the transcript's cwd and branch, and where it
/// came from.
fn header(
    transcript: &Transcript,
    chain: &[&Entry],
    source: &Source<'_>,
    leaf_uuid: &str,
    new_session_id: &str,
    now: &str,
) -> FileEntry {
    let mut imported = json!({
        "source": crate::SOURCE_ID,
        "sessionId": source.session_id,
        "path": source.path,
        "leafUuid": leaf_uuid,
    });
    if let Some(version) = chain
        .iter()
        .rev()
        .find_map(|entry| entry.version.as_deref())
    {
        imported["version"] = Value::from(version);
    }
    let mut rest = Map::default();
    rest.insert(IMPORTED_FROM_KEY.to_string(), imported);
    FileEntry::Header {
        header: SessionHeader {
            version: Some(pa_core::session::CURRENT_SESSION_VERSION),
            id: new_session_id.to_string(),
            timestamp: now.to_string(),
            cwd: transcript.cwd().unwrap_or("/").to_string(),
            parent_session: None,
            rlm_depth: Some(0),
            git: transcript.git_branch().map(|branch| GitContext {
                repo_url: None,
                commit: None,
                branch: Some(branch.to_string()),
            }),
            rest,
        },
    }
}

/// Convert the transcript's main conversation. `None` when it has none.
pub(crate) fn convert(
    transcript: &Transcript,
    source: &Source<'_>,
    new_session_id: &str,
    now: &str,
) -> Option<Converted> {
    let leaf = transcript.leaf()?;
    let chain = transcript.chain(leaf);
    let mut walk = Walk {
        builder: Builder {
            entries: Vec::new(),
            last_id: None,
            fallback_timestamp: now.to_string(),
        },
        results: tool_results(transcript),
        groups: reply_groups(transcript),
        pending_boundary: None,
        emitted_replies: HashSet::new(),
    };
    for entry in &chain {
        walk.entry(entry);
    }
    walk.flush_boundary();
    let mut builder = walk.builder;
    if let Some(title) = transcript.title() {
        let base = builder.base(Some(now));
        builder.entries.push(FileEntry::SessionInfo {
            payload: SessionInfoEntry {
                name: Some(title.to_string()),
            },
            base,
        });
    }
    builder.push_custom(
        Some(now),
        IMPORT_NOTE_CUSTOM_TYPE,
        format!(
            "This conversation was imported from Claude Code session {} ({}). The Claude Code \
             tool calls and results above ran in Claude Code and are kept as text records; \
             continue the work with this session's own tools.",
            source.session_id, source.path
        ),
    );
    let mut entries = Vec::with_capacity(builder.entries.len() + 1);
    entries.push(header(
        transcript,
        &chain,
        source,
        &leaf.uuid,
        new_session_id,
        now,
    ));
    entries.append(&mut builder.entries);
    Some(Converted {
        entries,
        cwd: transcript.cwd().unwrap_or("/").to_string(),
        leaf_uuid: leaf.uuid.clone(),
    })
}
