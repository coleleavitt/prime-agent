//! The `ctx.*` host requests the bundled `ctx` skill sends.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use serde_json::{Value, json};

use crate::message::{self, Kind};
use crate::session::ContextSession;
use crate::transform;

/// `ctx.status`.
pub(crate) const STATUS_REQUEST: &str = "ctx.status";
/// `ctx.reduce`.
pub(crate) const REDUCE_REQUEST: &str = "ctx.reduce";
/// `ctx.expand`.
pub(crate) const EXPAND_REQUEST: &str = "ctx.expand";
/// `ctx.flush`.
pub(crate) const FLUSH_REQUEST: &str = "ctx.flush";

/// The most tags one `ctx.reduce` accepts.
const MAX_REDUCE: usize = 200;
/// The most characters `ctx.expand` returns (~15k tokens).
const EXPAND_CHARS: usize = 60_000;

/// The tags of a `"3-5,9"` spec.
pub(crate) fn parse_spec(spec: &str) -> anyhow::Result<BTreeSet<u32>> {
    let mut tags = BTreeSet::new();
    for part in spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let number = |text: &str| -> anyhow::Result<u32> {
            text.trim()
                .trim_matches('§')
                .parse()
                .with_context(|| format!("ctx.reduce: {text:?} is not a tag number"))
        };
        match part.split_once('-') {
            Some((start, end)) => {
                let (start, end) = (number(start)?, number(end)?);
                if start > end {
                    bail!("ctx.reduce: range {part} runs backwards");
                }
                if usize::try_from(end - start).unwrap_or(usize::MAX) >= MAX_REDUCE {
                    bail!("ctx.reduce: range {part} is too wide; name the outputs you used");
                }
                tags.extend(start..=end);
            }
            None => {
                tags.insert(number(part)?);
            }
        }
    }
    if tags.is_empty() {
        bail!("ctx.reduce needs tags, like \"3-5,9\"");
    }
    if tags.len() > MAX_REDUCE {
        bail!("ctx.reduce accepts at most {MAX_REDUCE} tags at once");
    }
    Ok(tags)
}

/// `ctx.status`.
pub(crate) fn status(session: &ContextSession) -> Value {
    let snapshot = session.snapshot();
    let state = session
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let report = snapshot.map(|snapshot| snapshot.report).unwrap_or_default();
    let mut biggest = report.outputs.clone();
    biggest.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.tag.cmp(&b.tag)));
    json!({
        "usageTokens": report.usage,
        "window": session.window,
        "usagePercent": (report.share() * 1000.0).round() / 10.0,
        "tags": state.last_tag,
        "dropped": state.dropped.len(),
        "queued": state.queued.iter().collect::<Vec<_>>(),
        "compartments": state.compartments.len(),
        "appliedCompartments": state.applied,
        "historian": {
            "running": session.historian_running(),
            "runs": state.historian.runs,
            "failures": state.historian.failures,
            "lastError": state.historian.last_error,
        },
        "biggestOutputs": biggest.iter().take(10).map(|output| json!({
            "tag": output.tag,
            "tokens": output.tokens,
            "preview": output.preview,
        })).collect::<Vec<_>>(),
        "protectedTags": report.newest_tags,
        "thresholds": {
            "applyPercent": transform::EXECUTE * 100.0,
            "historianPercent": transform::HISTORIAN_TRIGGER * 100.0,
            "emergencyPercent": transform::FORCE * 100.0,
        },
    })
}

/// `ctx.reduce`: queue drops for the next permitted request.
pub(crate) fn reduce(session: &ContextSession, data: &Value) -> anyhow::Result<Value> {
    let spec = data
        .get("tags")
        .and_then(Value::as_str)
        .context("ctx.reduce needs a string `tags`, like \"3-5,9\"")?;
    let wanted = parse_spec(spec)?;
    let protected: BTreeSet<u32> = session
        .snapshot()
        .map(|snapshot| snapshot.report.newest_tags.into_iter().collect())
        .unwrap_or_default();
    let (queued, skipped) = session.update(|state| {
        let mut queued = Vec::new();
        let mut skipped = Vec::new();
        for tag in wanted {
            let key = state.key_of(tag);
            let reason = match key {
                None => Some("unknown"),
                Some(key) if !key.starts_with("t:") => {
                    Some("user message: kept, it carries the user's instructions")
                }
                Some(_) if protected.contains(&tag) => Some("protected"),
                Some(_) if state.dropped.contains(&tag) => Some("already dropped"),
                Some(_) => None,
            };
            if let Some(reason) = reason {
                skipped.push(json!({ "tag": tag, "reason": reason }));
            } else if state.queued.insert(tag) {
                queued.push(tag);
            }
        }
        (queued, skipped)
    });
    Ok(json!({
        "queued": queued,
        "skipped": skipped,
        "appliesWhen": "the next request with a cold prompt cache (first after a restart or 5 minutes idle), at 65% usage, or after ctx.flush()",
    }))
}

/// `ctx.expand`: one item's original text, or a tag range's raw transcript.
pub(crate) fn expand(session: &ContextSession, data: &Value) -> anyhow::Result<Value> {
    let snapshot = session
        .snapshot()
        .context("ctx.expand: no request has run in this session yet")?;
    let raw = snapshot.raw.as_slice();
    let keys = message::keys(raw);
    let state = session
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let index_of = |tag: u32| -> anyhow::Result<usize> {
        let key = state
            .key_of(tag)
            .with_context(|| format!("ctx.expand: no tag §{tag}§"))?;
        keys.iter()
            .position(|candidate| candidate.as_deref() == Some(key))
            .with_context(|| format!("ctx.expand: §{tag}§ is no longer in this session's history"))
    };
    let tag_arg = |name: &str| {
        data.get(name)
            .and_then(Value::as_u64)
            .and_then(|tag| u32::try_from(tag).ok())
    };
    let (start, end) = match (tag_arg("tag"), tag_arg("start"), tag_arg("end")) {
        (Some(tag), _, _) => {
            let index = index_of(tag)?;
            return Ok(json!({ "tag": tag, "text": message::text(&raw[index]) }));
        }
        (None, Some(start), Some(end)) if start <= end => (index_of(start)?, index_of(end)?),
        _ => bail!("ctx.expand needs `tag`, or `start` <= `end`"),
    };
    let tags: Vec<Option<u32>> = raw[start..=end]
        .iter()
        .zip(&keys[start..=end])
        .map(|(message, key)| match (message::kind(message), key) {
            (Kind::User | Kind::ToolResult, Some(key)) => state.tags.get(key).copied(),
            _ => None,
        })
        .collect();
    let text = transform::transcript(&raw[start..=end], &tags, usize::MAX);
    let truncated = text.len() > EXPAND_CHARS;
    let text: String = if truncated {
        let mut cut: String = text.chars().take(EXPAND_CHARS).collect();
        cut.push_str("\n[truncated: expand a narrower range]");
        cut
    } else {
        text
    };
    Ok(json!({ "text": text, "truncated": truncated }))
}

/// `ctx.flush`: let the next request apply queued work.
pub(crate) fn flush(session: &ContextSession) -> Value {
    let queued = session.update(|state| {
        state.flush_requested = true;
        state.queued.len()
    });
    json!({ "queued": queued, "appliesOn": "the next request" })
}

/// One request's synchronous handler.
type Handler = fn(&ContextSession, &Value) -> anyhow::Result<Value>;

/// Register every `ctx.*` request for `session`.
pub(crate) fn register(session: &Arc<ContextSession>, handlers: &mut HostRequestHandlers) {
    let entries: [(&'static str, Handler); 4] = [
        (STATUS_REQUEST, |session, _| Ok(status(session))),
        (REDUCE_REQUEST, reduce),
        (EXPAND_REQUEST, expand),
        (FLUSH_REQUEST, |session, _| Ok(flush(session))),
    ];
    for (request, run) in entries {
        let session = Arc::clone(session);
        handlers.register(
            request,
            host_handler(move |payload| {
                let session = Arc::clone(&session);
                async move { run(&session, &payload.data) }
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::tests::{assistant, tool_result, user};

    fn session_with_history() -> Arc<ContextSession> {
        let session = Arc::new(ContextSession::with(
            "s1".to_string(),
            None,
            1_000_000,
            None,
        ));
        let mut messages = Vec::new();
        for turn in 0..5_i64 {
            let call = format!("c{turn}");
            messages.push(user(&format!("task {turn}"), turn * 10));
            messages.push(assistant("working", Some(&call), turn * 10 + 1));
            messages.push(tool_result(&call, &format!("output {turn}"), turn * 10 + 2));
        }
        session.transform(messages);
        session
    }

    #[test]
    fn specs_parse_and_refuse_blanket_ranges() {
        assert_eq!(
            parse_spec("3-5, 9,§12§").unwrap(),
            BTreeSet::from([3, 4, 5, 9, 12])
        );
        assert!(parse_spec("").is_err());
        assert!(parse_spec("5-3").is_err());
        assert!(parse_spec("1-500").is_err());
        assert!(parse_spec("x").is_err());
    }

    #[test]
    fn reduce_queues_known_unprotected_tags() {
        let session = session_with_history();
        let reply = reduce(&session, &json!({"tags": "2,3,4,10,99"})).unwrap();
        assert_eq!(reply["queued"], json!([2, 4]));
        let reasons: Vec<&str> = reply["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|skip| skip["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons,
            [
                "user message: kept, it carries the user's instructions",
                "protected",
                "unknown"
            ]
        );
        assert_eq!(flush(&session)["queued"], 2);
        assert!(session.state.lock().unwrap().flush_requested);
    }

    #[test]
    fn expand_returns_items_and_ranges() {
        let session = session_with_history();
        assert_eq!(
            expand(&session, &json!({"tag": 2})).unwrap()["text"],
            "output 0"
        );
        let range = expand(&session, &json!({"start": 1, "end": 3})).unwrap();
        let text = range["text"].as_str().unwrap();
        assert!(text.starts_with("§1§ U: task 0\nA: working"));
        assert!(text.contains("§2§ T: output 0") && text.contains("§3§ U: task 1"));
        assert!(expand(&session, &json!({"tag": 999})).is_err());
        assert!(expand(&session, &json!({})).is_err());
        let status = status(&session);
        assert_eq!(status["tags"], 10);
        assert_eq!(status["window"], 1_000_000);
    }
}
