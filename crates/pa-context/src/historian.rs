//! The historian: one side-model call folds a range of old history into
//! compartments with four tiers of detail. The prompt condenses
//! magic-context's compartment agent (`historian-prompt.source.md`: the
//! P1-P4 ladder, importance as a decay rate, anchors verbatim, first person,
//! no invention). A reply is published only whole: every compartment
//! parsed, in order, inside the range.

use std::sync::OnceLock;

use regex::Regex;

use crate::state::Compartment;
use crate::transform::Job;

/// The historian's system prompt.
pub(crate) const HISTORIAN_SYSTEM_PROMPT: &str = r#"You are the historian of a coding agent's session. You receive a contiguous span of the session's older transcript and fold it into compartments: consecutive episodes, each a coherent unit of work (one investigation, one fix, one decision). The agent will later read only your compartments instead of the raw span, so they must carry everything it may need.

Transcript lines: "§N§ U:" a user message with its tag N, "A:" the agent's reply or code cell ("[call python_repl] <code>"), "§N§ T:" a tool output with its tag N (long outputs are cut and end in "[...]").

Output only this XML, nothing before or after it:
<compartments>
<compartment start="N" end="M" title="short title" importance="1-100">
<p1>What happened, in full: every anchor kept verbatim (file paths, functions, commands, flags, error messages, numbers, commit hashes, URLs) and the user's own words quoted when they set direction.</p1>
<p2>The same episode with only its canonical anchors: what was done, what came out, the decision.</p2>
<p3>Outcome and key decision in one or two sentences.</p3>
<p4>One fragment: the single anchor worth remembering.</p4>
</compartment>
</compartments>

Rules:
- start and end are tag numbers from the transcript; compartments are in order, do not overlap, and together cover the whole span from its first tag to its last.
- Write in the first person as the agent ("I traced...", "the user asked me to...").
- importance is how slowly the episode should fade: 80-100 for user preferences, corrections, decisions and facts later work depends on; 40-60 for ordinary progress; 1-30 for dead ends and noise.
- Never invent anything the transcript does not show. Keep commit hashes, paths and numbers exact at every tier that mentions them.
- Never copy secrets, tokens, passwords or keys; write [secret] instead.
- Prefer fewer, larger compartments to many small ones: roughly one per 3-15 user turns."#;

/// The user message of one run.
pub(crate) fn prompt(job: &Job) -> String {
    format!(
        "Fold this span (tags §{}§ to §{}§) into compartments.\n\n<transcript>\n{}</transcript>",
        job.first_tag, job.last_tag, job.transcript
    )
}

fn compartment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?s)<compartment\s+([^>]*)>(.*?)</compartment>")
            .expect("the compartment pattern is a valid regex")
    })
}

fn attribute(attributes: &str, name: &str) -> Option<String> {
    let pattern = Regex::new(&format!(r#"\b{name}\s*=\s*"([^"]*)""#)).ok()?;
    pattern
        .captures(attributes)
        .map(|captures| captures[1].trim().to_string())
}

fn tier(body: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    body.find(&open)
        .and_then(|start| {
            let rest = &body[start + open.len()..];
            rest.find(&close).map(|end| rest[..end].trim().to_string())
        })
        .unwrap_or_default()
}

/// The compartments of `reply` for `job`: the first opens the chunk at its
/// start key, the last closes it at its end key.
///
/// # Errors
///
/// When the reply holds no compartment, one lacks its range or `<p1>`, the
/// ranges leave the span or run out of order.
pub(crate) fn parse(reply: &str, job: &Job) -> anyhow::Result<Vec<Compartment>> {
    let mut compartments = Vec::new();
    let mut previous_end = job.first_tag.saturating_sub(1);
    for captures in compartment_pattern().captures_iter(reply) {
        let (attributes, body) = (&captures[1], &captures[2]);
        let range = |name: &str| -> anyhow::Result<u32> {
            attribute(attributes, name)
                .and_then(|value| value.trim_matches('§').parse().ok())
                .ok_or_else(|| anyhow::anyhow!("a compartment has no numeric {name}"))
        };
        let (start, end) = (range("start")?, range("end")?);
        if start > end || (!compartments.is_empty() && start <= previous_end) || end > job.last_tag
        {
            anyhow::bail!(
                "compartment §{start}§-§{end}§ is out of order or outside §{}§-§{}§",
                job.first_tag,
                job.last_tag
            );
        }
        previous_end = end;
        let tiers = [
            tier(body, "p1"),
            tier(body, "p2"),
            tier(body, "p3"),
            tier(body, "p4"),
        ];
        if tiers[0].is_empty() {
            anyhow::bail!("compartment §{start}§-§{end}§ has no <p1>");
        }
        compartments.push(Compartment {
            start_key: None,
            end_key: None,
            start_tag: start,
            end_tag: end,
            title: attribute(attributes, "title").unwrap_or_else(|| format!("§{start}§-§{end}§")),
            importance: attribute(attributes, "importance")
                .and_then(|value| value.parse().ok())
                .unwrap_or(50)
                .clamp(1, 100),
            created_ms: 0,
            tiers,
        });
    }
    let Some(first) = compartments.first_mut() else {
        anyhow::bail!("the historian returned no compartment");
    };
    first.start_key = Some(job.start_key.clone());
    first.start_tag = job.first_tag;
    if let Some(last) = compartments.last_mut() {
        last.end_key = Some(job.end_key.clone());
        last.end_tag = job.last_tag;
    }
    Ok(compartments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            after: None,
            start_key: "u:10".to_string(),
            end_key: "a:99".to_string(),
            first_tag: 3,
            last_tag: 20,
            tokens: 9_000,
            transcript: "§3§ U: go\n".to_string(),
        }
    }

    #[test]
    fn a_whole_reply_publishes_with_the_chunk_keys() {
        let reply = r#"Sure.
<compartments>
<compartment start="3" end="9" title="Traced the lease" importance="80">
<p1>I traced writer_lease_timeout in shim.ts:120 to the shared XDG dir.</p1>
<p2>Traced the lease timeout to the shared dir.</p2><p3>Shared dir caused it.</p3><p4>lease</p4>
</compartment>
<compartment start="§10§" end="18" title="Fixed it" importance="300">
<p1>I added per-pid dirs (commit abc1234).</p1>
</compartment>
</compartments>"#;
        let compartments = parse(reply, &job()).unwrap();
        assert_eq!(compartments.len(), 2);
        assert_eq!(compartments[0].start_key.as_deref(), Some("u:10"));
        assert_eq!(compartments[0].end_key, None);
        assert_eq!(compartments[1].end_key.as_deref(), Some("a:99"));
        assert_eq!(
            compartments[1].end_tag, 20,
            "the last closes the whole span"
        );
        assert_eq!(compartments[1].importance, 100);
        assert_eq!(compartments[0].tiers[3], "lease");
        assert!(prompt(&job()).contains("§3§ to §20§"));
    }

    #[test]
    fn broken_replies_publish_nothing() {
        assert!(parse("no xml here", &job()).is_err());
        let backwards = r#"<compartment start="9" end="5"><p1>x</p1></compartment>"#;
        assert!(parse(backwards, &job()).is_err());
        let outside = r#"<compartment start="3" end="99"><p1>x</p1></compartment>"#;
        assert!(parse(outside, &job()).is_err());
        let overlap = r#"<compartment start="3" end="9"><p1>x</p1></compartment><compartment start="8" end="12"><p1>y</p1></compartment>"#;
        assert!(parse(overlap, &job()).is_err());
        let empty = r#"<compartment start="3" end="9"><p2>x</p2></compartment>"#;
        assert!(parse(empty, &job()).is_err());
    }
}
