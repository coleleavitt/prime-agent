//! Listing, finding and importing Claude Code transcripts, over hand-built
//! fixtures in Claude Code 2.1.296's format (never real transcripts).

use std::path::{Path, PathBuf};

use pa_cc_import::{
    ClaudeCodeSessions,
    IMPORT_NOTE_CUSTOM_TYPE,
    IMPORTED_FROM_KEY,
    TOOL_RESULT_CUSTOM_TYPE,
    find,
    import_transcript,
    list_all,
    list_project,
    project_dir,
};
use pa_core::session::build_session_context;
use pa_core::session::external_sessions::{ExternalMatch, ExternalScope, ExternalSessionSource};
use pa_core::session::manager::load_entries_from_file;
use pa_types::ai::{AssistantContentBlock, UserContent, UserContentBlock};
use pa_types::session::{AgentMessage, FileEntry};
use serde_json::{Value, json};

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const SIDE: &str = "33333333-3333-4333-8333-333333333333";
const EMPTY: &str = "44444444-4444-4444-8444-444444444444";
const OTHER: &str = "55555555-5555-4555-8555-555555555555";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    elsewhere: PathBuf,
    sessions: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("claude").join("projects");
    let cwd = tmp.path().join("work").join("my.repo");
    let elsewhere = tmp.path().join("work").join("other");
    let sessions = tmp.path().join("sessions");
    for dir in [&root, &cwd, &elsewhere, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
    }
    Fixture {
        _tmp: tmp,
        root,
        cwd,
        elsewhere,
        sessions,
    }
}

fn write(root: &Path, cwd: &Path, id: &str, lines: &[Value]) -> PathBuf {
    let dir = project_dir(root, cwd);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let text = lines.iter().fold(String::new(), |mut text, line| {
        text.push_str(&line.to_string());
        text.push('\n');
        text
    });
    std::fs::write(&path, text).unwrap();
    path
}

fn at(second: u32) -> String {
    format!("2026-03-01T10:00:{second:02}.000Z")
}

fn envelope(kind: &str, uuid: &str, parent: Option<&str>, second: u32, cwd: &Path) -> Value {
    json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "isSidechain": false,
        "userType": "external", "cwd": cwd.to_string_lossy(), "sessionId": S1,
        "version": "2.1.296", "gitBranch": "feature/x", "timestamp": at(second),
    })
}

fn user(uuid: &str, parent: Option<&str>, second: u32, cwd: &Path, content: Value) -> Value {
    let mut line = envelope("user", uuid, parent, second, cwd);
    line["message"] = json!({ "role": "user" });
    line["message"]["content"] = content;
    line
}

fn assistant(
    uuid: &str,
    parent: &str,
    second: u32,
    cwd: &Path,
    message_id: &str,
    block: Value,
) -> Value {
    let mut line = envelope("assistant", uuid, Some(parent), second, cwd);
    line["requestId"] = json!("req_1");
    line["message"] = json!({
        "id": message_id, "type": "message", "role": "assistant", "model": "claude-opus-4-7",
        "content": [], "stop_reason": null,
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    });
    line["message"]["content"] = Value::Array(vec![block]);
    line
}

/// A session with a slash command, a prompt with an image, a streamed reply
/// (thinking, text, two parallel tool calls on sibling entries), their
/// results, a compaction, and a final exchange.
#[expect(
    clippy::too_many_lines,
    reason = "one fixture transcript, line by line in file order"
)]
fn rich_session(cwd: &Path) -> Vec<Value> {
    let mut meta = user(
        "m0",
        None,
        0,
        cwd,
        json!("Caveat: local command output follows"),
    );
    meta["isMeta"] = json!(true);
    let mut boundary = envelope("system", "b1", None, 20, cwd);
    boundary["subtype"] = json!("compact_boundary");
    boundary["logicalParentUuid"] = json!("a3");
    boundary["content"] = json!("Conversation compacted");
    boundary["compactMetadata"] = json!({ "trigger": "manual", "preTokens": 54321 });
    let mut summary = user(
        "u3",
        Some("b1"),
        21,
        cwd,
        json!("This session is being continued from a previous conversation. Summary: X."),
    );
    summary["isCompactSummary"] = json!(true);
    let mut synthetic = assistant(
        "e1",
        "u4",
        31,
        cwd,
        "msg_err",
        json!({ "type": "text", "text": "API Error: overloaded" }),
    );
    synthetic["message"]["model"] = json!("<synthetic>");
    synthetic["isApiErrorMessage"] = json!(true);
    vec![
        json!({ "type": "file-history-snapshot", "messageId": "m0", "snapshot": {}, "isSnapshotUpdate": false }),
        meta,
        user(
            "u1",
            Some("m0"),
            1,
            cwd,
            json!([
                { "type": "text", "text": "Fix the parser" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0=" } },
            ]),
        ),
        assistant(
            "a1",
            "u1",
            2,
            cwd,
            "msg_1",
            json!({ "type": "thinking", "thinking": "hmm", "signature": "sig" }),
        ),
        assistant(
            "a2",
            "a1",
            3,
            cwd,
            "msg_1",
            json!({ "type": "text", "text": "Looking." }),
        ),
        assistant(
            "a3",
            "a2",
            4,
            cwd,
            "msg_1",
            json!({ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls src", "description": "List" } }),
        ),
        // The parallel call sits on a sibling branch (both children of a2).
        assistant(
            "a4",
            "a2",
            4,
            cwd,
            "msg_1",
            json!({ "type": "tool_use", "id": "toolu_2", "name": "Read", "input": { "file_path": "/w/src/lib.rs" } }),
        ),
        {
            let mut result = user(
                "r1",
                Some("a3"),
                5,
                cwd,
                json!([{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "lib.rs\nmain.rs" }]),
            );
            result["toolUseResult"] = json!({ "stdout": "lib.rs\nmain.rs", "stderr": "" });
            result
        },
        user(
            "r2",
            Some("a4"),
            5,
            cwd,
            json!([{ "type": "tool_result", "tool_use_id": "toolu_2", "is_error": true,
                     "content": [{ "type": "text", "text": "File does not exist." }] }]),
        ),
        json!({ "type": "progress", "uuid": "p1", "parentUuid": "r1", "data": {} }),
        boundary,
        summary,
        assistant(
            "a5",
            "u3",
            22,
            cwd,
            "msg_2",
            json!({ "type": "text", "text": "Continuing." }),
        ),
        user("u4", Some("a5"), 30, cwd, json!("Now run the tests")),
        synthetic,
        assistant(
            "a6",
            "e1",
            32,
            cwd,
            "msg_3",
            json!({ "type": "text", "text": "All green." }),
        ),
        json!({ "type": "ai-title", "aiTitle": "Parser fix", "sessionId": S1 }),
        json!({ "type": "last-prompt", "lastPrompt": "Now run the tests", "leafUuid": "a6", "sessionId": S1 }),
        json!({ "type": "some-new-entry-type", "sessionId": S1, "payload": [1, 2, 3] }),
    ]
}

#[test]
fn a_project_lists_its_sessions_newest_first_and_skips_the_rest() {
    let fx = fixture();
    write(&fx.root, &fx.cwd, S1, &rich_session(&fx.cwd));
    write(
        &fx.root,
        &fx.cwd,
        S2,
        &[
            user("x1", None, 50, &fx.cwd, json!("Second session prompt")),
            json!({ "type": "summary", "summary": "A summary title", "leafUuid": "x1" }),
            json!({ "type": "custom-title", "customTitle": "My custom", "sessionId": S2 }),
        ],
    );
    // A subagent transcript and a bookkeeping-only file never list.
    let mut side = user("s1", None, 60, &fx.cwd, json!("subagent task"));
    side["isSidechain"] = json!(true);
    write(&fx.root, &fx.cwd, SIDE, &[side]);
    write(
        &fx.root,
        &fx.cwd,
        EMPTY,
        &[json!({ "type": "file-history-snapshot", "messageId": "z", "snapshot": {} })],
    );
    // Another project's session lists only under all projects.
    write(
        &fx.root,
        &fx.elsewhere,
        OTHER,
        &[user("o1", None, 40, &fx.elsewhere, json!("Elsewhere"))],
    );

    let rows = list_project(&fx.root, &fx.cwd);
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, [S2, S1]);
    let rich = &rows[1];
    assert_eq!(rich.title.as_deref(), Some("Parser fix"));
    assert_eq!(rich.prompt.as_deref(), Some("Now run the tests"));
    assert_eq!(rich.git_branch.as_deref(), Some("feature/x"));
    assert_eq!(rich.created, at(0));
    assert_eq!(rich.modified, at(32));
    assert_eq!(rich.cwd, fx.cwd.to_string_lossy());
    // Claude Code's count over the conversation path: u1, a2 (text), the
    // compact summary u3, a5, u4, the API error e1 and a6. Meta rows,
    // thinking, tool calls, the boundary and the off-path results do not.
    assert_eq!(rich.message_count, 7);
    // The custom title outranks the summary.
    assert_eq!(rows[0].title.as_deref(), Some("My custom"));
    assert_eq!(rows[0].message_count, 1);

    let all: Vec<String> = list_all(&fx.root).into_iter().map(|row| row.id).collect();
    assert_eq!(all, [S2.to_string(), OTHER.to_string(), S1.to_string()]);

    let source = ClaudeCodeSessions {
        root: Some(fx.root.clone()),
    };
    let scoped = source.list(&ExternalScope::Directory(fx.cwd));
    assert_eq!(scoped.len(), 2);
    assert_eq!(scoped[0].source, "claude-code");
    assert_eq!(scoped[0].source_label, "Claude Code");
    assert_eq!(source.list(&ExternalScope::All).len(), 3);
}

#[test]
fn a_same_named_directory_does_not_list_another_cwds_sessions() {
    let fx = fixture();
    // `my.repo` and `my-repo` sanitize to the same project directory.
    let twin = fx.cwd.with_file_name("my-repo");
    std::fs::create_dir_all(&twin).unwrap();
    write(
        &fx.root,
        &twin,
        S2,
        &[user("t1", None, 5, &twin, json!("twin"))],
    );
    write(
        &fx.root,
        &fx.cwd,
        S1,
        &[user("u1", None, 1, &fx.cwd, json!("mine"))],
    );
    let ids: Vec<String> = list_project(&fx.root, &fx.cwd)
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(ids, [S1.to_string()]);
}

#[test]
fn a_selector_finds_by_id_prefix_and_title() {
    let fx = fixture();
    write(&fx.root, &fx.cwd, S1, &rich_session(&fx.cwd));
    write(
        &fx.root,
        &fx.elsewhere,
        OTHER,
        &[
            user("o1", None, 40, &fx.elsewhere, json!("Elsewhere")),
            json!({ "type": "custom-title", "customTitle": "Far Away", "sessionId": OTHER }),
        ],
    );
    let one = |found: ExternalMatch| match found {
        ExternalMatch::One(row) => row.id,
        other => panic!("expected one match, got {other:?}"),
    };
    // A full id finds the session in any project.
    assert_eq!(one(find(&fx.root, OTHER, &fx.cwd)), OTHER);
    assert_eq!(one(find(&fx.root, &S1[..8], &fx.cwd)), S1);
    assert_eq!(one(find(&fx.root, "  parser FIX ", &fx.cwd)), S1);
    assert_eq!(one(find(&fx.root, "far away", &fx.cwd)), OTHER);
    assert_eq!(
        find(&fx.root, "no such title", &fx.cwd),
        ExternalMatch::None
    );
}

fn import(
    fx: &Fixture,
    transcript: &Path,
) -> (
    pa_core::session::external_sessions::ImportedSession,
    Vec<FileEntry>,
) {
    let imported = import_transcript(transcript, &fx.sessions).unwrap();
    let entries = load_entries_from_file(&imported.path, false);
    (imported, entries)
}

fn text_of(blocks: &[AssistantContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .map(|block| match block {
            AssistantContentBlock::Text(text) => text.text.clone(),
            other => panic!("unexpected block {other:?}"),
        })
        .collect()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "walks the converted file entry by entry against the fixture"
)]
fn an_import_converts_the_conversation_and_leaves_the_transcript_untouched() {
    let fx = fixture();
    let transcript = write(&fx.root, &fx.cwd, S1, &rich_session(&fx.cwd));
    let before = std::fs::read(&transcript).unwrap();
    let (imported, entries) = import(&fx, &transcript);
    assert!(!imported.reused);
    assert_eq!(imported.cwd, fx.cwd.to_string_lossy());
    assert_eq!(
        std::fs::read(&transcript).unwrap(),
        before,
        "the transcript is never written"
    );
    assert_eq!(imported.path.parent(), Some(fx.sessions.as_path()));

    let FileEntry::Header { header } = &entries[0] else {
        panic!("the header leads");
    };
    assert_eq!(header.id, imported.session_id);
    assert_eq!(header.cwd, fx.cwd.to_string_lossy());
    assert_eq!(
        header.git.as_ref().and_then(|git| git.branch.as_deref()),
        Some("feature/x")
    );
    assert_eq!(
        header.rest[IMPORTED_FROM_KEY],
        json!({
            "source": "claude-code",
            "sessionId": S1,
            "path": transcript.to_string_lossy(),
            "leafUuid": "a6",
            "version": "2.1.296",
        })
    );

    let body = &entries[1..];
    // user, assistant, tool results, compaction, assistant, user, assistant,
    // session name, import note.
    let FileEntry::Message {
        message: AgentMessage::User(prompt),
        base,
    } = &body[0]
    else {
        panic!("the first prompt: {:?}", body[0]);
    };
    assert_eq!(base.parent_id, None);
    assert_eq!(base.timestamp.as_deref(), Some(at(1).as_str()));
    let UserContent::Blocks(blocks) = &prompt.content else {
        panic!("blocks");
    };
    assert!(matches!(&blocks[0], UserContentBlock::Text(text) if text.text == "Fix the parser"));
    assert!(matches!(&blocks[1], UserContentBlock::Image(image) if image.mime_type == "image/png"));

    let FileEntry::Message {
        message: AgentMessage::Assistant(reply),
        ..
    } = &body[1]
    else {
        panic!("the reply: {:?}", body[1]);
    };
    assert_eq!(reply.provider, "anthropic");
    assert_eq!(reply.model, "claude-opus-4-7");
    assert_eq!(reply.usage.total_tokens, 0);
    // Thinking dropped; both parallel calls kept, the sibling included.
    assert_eq!(
        text_of(&reply.content),
        [
            "Looking.".to_string(),
            "[Claude Code tool call: Bash]\n$ ls src".to_string(),
            "[Claude Code tool call: Read]\n{\"file_path\":\"/w/src/lib.rs\"}".to_string(),
        ]
    );

    let FileEntry::CustomMessage { payload, .. } = &body[2] else {
        panic!("the tool results: {:?}", body[2]);
    };
    assert_eq!(payload.custom_type, TOOL_RESULT_CUSTOM_TYPE);
    assert!(payload.display);
    assert_eq!(
        payload.content.text(),
        "[Claude Code tool result: Bash]\nlib.rs\nmain.rs\n\n\
         [Claude Code tool error: Read]\nFile does not exist."
    );

    let FileEntry::Compaction { payload, base } = &body[3] else {
        panic!("the compaction: {:?}", body[3]);
    };
    assert_eq!(payload.tokens_before, 54321);
    assert!(
        payload
            .summary
            .starts_with("This session is being continued")
    );
    assert_eq!(base.timestamp.as_deref(), Some(at(20).as_str()));

    let kinds: Vec<String> = body[4..]
        .iter()
        .map(|entry| match entry {
            FileEntry::Message {
                message: AgentMessage::User(user),
                ..
            } => format!("user:{}", user.content.text()),
            FileEntry::Message {
                message: AgentMessage::Assistant(reply),
                ..
            } => format!("assistant:{}", text_of(&reply.content).join("|")),
            FileEntry::SessionInfo { payload, .. } => {
                format!("name:{}", payload.name.clone().unwrap_or_default())
            }
            FileEntry::CustomMessage { payload, .. } => format!("custom:{}", payload.custom_type),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "assistant:Continuing.".to_string(),
            "user:Now run the tests".to_string(),
            "assistant:All green.".to_string(),
            "name:Parser fix".to_string(),
            format!("custom:{IMPORT_NOTE_CUSTOM_TYPE}"),
        ]
    );
    // One linear chain.
    for pair in body.windows(2) {
        assert_eq!(pair[1].parent_id(), pair[0].id());
    }

    // The model's view: the compaction summary, then what followed.
    let context = build_session_context(&entries, None);
    assert!(matches!(
        &context.messages[0],
        AgentMessage::CompactionSummary(summary) if summary.retained_message_count == Some(0)
    ));
    assert_eq!(context.messages.len(), 5);
    assert_eq!(
        context.model,
        Some(("anthropic".to_string(), "claude-opus-4-7".to_string()))
    );
}

#[test]
fn reopening_an_unchanged_session_reuses_its_import_and_a_grown_one_imports_again() {
    let fx = fixture();
    let mut lines = rich_session(&fx.cwd);
    let transcript = write(&fx.root, &fx.cwd, S1, &lines);
    let (first, _) = import(&fx, &transcript);
    let (again, _) = import(&fx, &transcript);
    assert!(again.reused);
    assert_eq!(again.path, first.path);
    assert_eq!(again.session_id, first.session_id);

    lines.push(user("u5", Some("a6"), 40, &fx.cwd, json!("one more thing")));
    write(&fx.root, &fx.cwd, S1, &lines);
    let (grown, entries) = import(&fx, &transcript);
    assert!(!grown.reused);
    assert_ne!(grown.path, first.path);
    let FileEntry::Header { header } = &entries[0] else {
        panic!("header");
    };
    assert_eq!(header.rest[IMPORTED_FROM_KEY]["leafUuid"], json!("u5"));
}

#[test]
fn the_source_imports_by_id_and_refuses_what_it_cannot() {
    let fx = fixture();
    write(&fx.root, &fx.cwd, S1, &rich_session(&fx.cwd));
    write(
        &fx.root,
        &fx.cwd,
        EMPTY,
        &[json!({ "type": "file-history-snapshot", "messageId": "z", "snapshot": {} })],
    );
    let source = ClaudeCodeSessions {
        root: Some(fx.root.clone()),
    };
    let imported = source.import(S1, &fx.sessions).unwrap();
    assert!(imported.path.is_file());
    assert_eq!(
        source.import(S2, &fx.sessions).unwrap_err(),
        format!("Unknown Claude Code session {S2}")
    );
    assert_eq!(
        source.import(EMPTY, &fx.sessions).unwrap_err(),
        format!("Claude Code session {EMPTY} has no conversation to continue")
    );
    assert_eq!(
        source.import("../../etc/passwd", &fx.sessions).unwrap_err(),
        "Not a Claude Code session id: ../../etc/passwd"
    );
}

#[test]
fn a_session_without_a_compaction_keeps_every_message_in_context() {
    let fx = fixture();
    let transcript = write(
        &fx.root,
        &fx.cwd,
        S2,
        &[
            user("u1", None, 1, &fx.cwd, json!("hello")),
            assistant(
                "a1",
                "u1",
                2,
                &fx.cwd,
                "m1",
                json!({ "type": "text", "text": "hi" }),
            ),
            assistant(
                "a2",
                "a1",
                3,
                &fx.cwd,
                "m1",
                json!({ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "true" } }),
            ),
            // Interrupted: the call has no recorded result.
            user(
                "u2",
                Some("a2"),
                4,
                &fx.cwd,
                json!([{ "type": "text", "text": "[Request interrupted by user for tool use]" }]),
            ),
        ],
    );
    let (_, entries) = import(&fx, &transcript);
    let context = build_session_context(&entries, None);
    let roles: Vec<&str> = context
        .messages
        .iter()
        .map(|message| match message {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::Custom(custom) => custom.custom_type.as_str(),
            _ => "other",
        })
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", IMPORT_NOTE_CUSTOM_TYPE]
    );
    // No title: the session is not named.
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, FileEntry::SessionInfo { .. }))
    );
}
