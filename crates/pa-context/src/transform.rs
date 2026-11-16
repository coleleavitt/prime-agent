//! The per-request pipeline: tags, the rebuild permission, compartments,
//! drops, the emergency drop, and the historian's next range. Pure over the
//! messages and the session state, so a request that changes nothing
//! replays the previous request's bytes (the provider's prompt cache).

use std::fmt::Write as _;

use pa_agent::types::{AgentMessage, Message};

use crate::message::{self, Kind};
use crate::render;
use crate::state::{ContextState, Frozen};

/// Usage at or above which queued work applies (magic-context's execute
/// threshold).
pub(crate) const EXECUTE: f64 = 0.65;
/// Usage at or above which the emergency drop runs.
pub(crate) const FORCE: f64 = 0.85;
/// The usage the emergency drop aims below.
const EMERGENCY_TARGET: f64 = 0.75;
/// Usage at or above which the historian folds older history (execute
/// minus 2 points, so a fold is ready when queued work applies).
pub(crate) const HISTORIAN_TRIGGER: f64 = 0.63;
/// The share of the window the rendered history may take.
const HISTORY_BUDGET: f64 = 0.15;
/// The share of the history budget fresh compartments may take before a
/// permitted pass refolds them into the frozen part.
const REFOLD_SHARE: f64 = 0.2;
/// A historian range needs at least this many messages...
const MIN_RANGE_MESSAGES: usize = 12;
/// ...and this many tokens.
const MIN_RANGE_TOKENS: usize = 6_000;
/// The most tokens one historian run reads.
const MAX_RANGE_TOKENS: usize = 100_000;
/// The longest tool output the historian reads per item.
const TRANSCRIPT_ITEM_CHARS: usize = 2_000;

/// What a request knows besides the messages.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inputs {
    /// The model's context window in tokens.
    pub(crate) window: usize,
    /// The provider's cache is cold anyway (first request of the process,
    /// or idle past the cache lifetime): queued work may apply.
    pub(crate) cold: bool,
}

/// One undropped tool output, for nudges and `ctx.status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Output {
    pub(crate) tag: u32,
    pub(crate) tokens: usize,
    pub(crate) preview: String,
}

/// A range for the historian to fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Job {
    /// The end key of the last published compartment when the range was
    /// chosen: a reply publishes only while it still is.
    pub(crate) after: Option<String>,
    pub(crate) start_key: String,
    pub(crate) end_key: String,
    pub(crate) first_tag: u32,
    pub(crate) last_tag: u32,
    pub(crate) tokens: usize,
    pub(crate) transcript: String,
}

/// What one request saw.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Report {
    pub(crate) usage: usize,
    pub(crate) window: usize,
    /// Whether queued work applied on this request.
    pub(crate) permitted: bool,
    /// Tokens of the messages the history does not cover.
    pub(crate) tail_tokens: usize,
    /// Undropped tool outputs outside the history, oldest first.
    pub(crate) outputs: Vec<Output>,
    /// Tags the emergency drop added.
    pub(crate) emergency_dropped: Vec<u32>,
    /// The newest tags (protected from `ctx.reduce`).
    pub(crate) newest_tags: Vec<u32>,
    pub(crate) job: Option<Job>,
}

impl Report {
    /// Usage as a share of the window.
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts fit an f64 exactly"
    )]
    pub(crate) fn share(&self) -> f64 {
        if self.window == 0 {
            0.0
        } else {
            self.usage as f64 / self.window as f64
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "token arithmetic on window-sized values; results are clamped to the window"
)]
fn scaled(window: usize, share: f64) -> usize {
    (window as f64 * share).max(0.0).round() as usize
}

/// The request's token estimate: the last reply's measured prompt (input and
/// cache tokens) plus the estimate of what came after it, else the estimate
/// of everything.
fn usage(messages: &[AgentMessage], tokens: &[usize]) -> usize {
    let last = messages.iter().rposition(|message| {
        matches!(message, AgentMessage::Standard(Message::Assistant(assistant))
            if assistant.usage.input + assistant.usage.cache_read + assistant.usage.cache_write > 0)
    });
    match last {
        Some(index) => {
            let AgentMessage::Standard(Message::Assistant(assistant)) = &messages[index] else {
                unreachable!("rposition matched an assistant message");
            };
            let measured =
                assistant.usage.input + assistant.usage.cache_read + assistant.usage.cache_write;
            usize::try_from(measured).unwrap_or(usize::MAX) + tokens[index..].iter().sum::<usize>()
        }
        None => tokens.iter().sum(),
    }
}

/// The index range the applied compartments cover in `keys`, when both ends
/// are present in order.
fn cut(keys: &[Option<String>], state: &ContextState) -> Option<(usize, usize)> {
    let applied = &state.compartments[..state.applied.min(state.compartments.len())];
    let start_key = applied.iter().find_map(|c| c.start_key.as_deref())?;
    let end_key = applied.iter().rev().find_map(|c| c.end_key.as_deref())?;
    let position = |wanted: &str| keys.iter().position(|key| key.as_deref() == Some(wanted));
    let (start, end) = (position(start_key)?, position(end_key)?);
    (start <= end).then_some((start, end))
}

/// Whether `index` lies inside the applied history's range.
fn inside(cut: Option<(usize, usize)>, index: usize) -> bool {
    cut.is_some_and(|(start, end)| (start..=end).contains(&index))
}

/// At the force band with nothing queued: drop the oldest tool outputs
/// outside the history and the protected tail until the estimate is under
/// the emergency target.
fn emergency_drop(
    messages: &[AgentMessage],
    tokens: &[usize],
    tags: &[Option<u32>],
    cut: Option<(usize, usize)>,
    state: &mut ContextState,
    report: &mut Report,
) {
    let target = scaled(report.window, EMERGENCY_TARGET);
    let protected = scaled(report.window, 0.08).max(8_000);
    let mut tail = 0;
    let protected_from = (0..messages.len())
        .rev()
        .find(|&index| {
            tail += tokens[index];
            tail >= protected
        })
        .unwrap_or(0);
    let mut estimate = report.usage;
    for index in 0..protected_from {
        if estimate <= target {
            break;
        }
        if inside(cut, index) || message::kind(&messages[index]) != Kind::ToolResult {
            continue;
        }
        if let Some(tag) = tags[index]
            && state.dropped.insert(tag)
        {
            estimate = estimate.saturating_sub(tokens[index]);
            report.emergency_dropped.push(tag);
        }
    }
}

/// The history message text of the applied compartments: the frozen
/// decay-rendered part (refolded only when there is none yet, or on a
/// permitted pass with a cold cache or too many fresh compartments), then
/// the fresh ones.
fn history_text(state: &mut ContextState, inputs: &Inputs, permitted: bool) -> String {
    let applied = state.applied.min(state.compartments.len());
    let budget = scaled(inputs.window, HISTORY_BUDGET);
    let fresh_from = state
        .frozen
        .as_ref()
        .map_or(0, |frozen| frozen.covers.min(applied));
    let fresh_tokens: usize = state.compartments[fresh_from..applied]
        .iter()
        .map(|compartment| compartment.tiers[0].len() / 4)
        .sum();
    let refold = state
        .frozen
        .as_ref()
        .is_none_or(|frozen| frozen.covers > applied)
        || (permitted && (inputs.cold || fresh_tokens > scaled(budget, REFOLD_SHARE)));
    if refold {
        #[expect(clippy::cast_precision_loss, reason = "a token budget fits an f64")]
        let budget = budget as f64;
        state.frozen = Some(Frozen {
            text: render::decayed(&state.compartments[..applied], budget),
            covers: applied,
        });
    }
    let (frozen, covers) = state.frozen.as_ref().map_or((String::new(), 0), |frozen| {
        (frozen.text.clone(), frozen.covers)
    });
    render::history(
        &frozen,
        &render::fresh(&state.compartments[covers..applied]),
    )
}

/// Run the pipeline over `messages`.
pub(crate) fn run(
    messages: &[AgentMessage],
    state: &mut ContextState,
    inputs: &Inputs,
) -> (Vec<AgentMessage>, Report) {
    let keys = message::keys(messages);
    let tokens: Vec<usize> = messages.iter().map(message::tokens).collect();
    let tags: Vec<Option<u32>> = messages
        .iter()
        .zip(&keys)
        .map(|(message, key)| match (message::kind(message), key) {
            (Kind::User | Kind::ToolResult, Some(key)) => Some(state.tag_for(key)),
            _ => None,
        })
        .collect();

    let mut report = Report {
        usage: usage(messages, &tokens),
        window: inputs.window,
        ..Report::default()
    };
    let share = report.share();
    report.permitted = inputs.cold || state.flush_requested || share >= EXECUTE;
    if report.permitted {
        let queued = std::mem::take(&mut state.queued);
        state.dropped.extend(queued);
        state.applied = state.compartments.len();
        state.flush_requested = false;
    }

    let cut = cut(&keys, state);
    if share >= FORCE && state.queued.is_empty() {
        emergency_drop(messages, &tokens, &tags, cut, state, &mut report);
    }
    let history = cut.map(|_| history_text(state, inputs, report.permitted));

    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if let Some((start, _)) = cut
            && inside(cut, index)
        {
            if index == start
                && let Some(history) = &history
            {
                out.push(message::synthetic_user(
                    history.clone(),
                    message::timestamp(message),
                ));
            }
            continue;
        }
        out.push(match tags[index] {
            // Only tool outputs drop: a user message carries instructions the
            // model must keep seeing (an older state may still list one).
            Some(tag)
                if state.dropped.contains(&tag) && message::kind(message) == Kind::ToolResult =>
            {
                message::dropped(message.clone(), tag)
            }
            Some(tag) => message::tagged(message.clone(), tag),
            None => message.clone(),
        });
        if message::kind(message) == Kind::ToolResult
            && let Some(tag) = tags[index]
            && !state.dropped.contains(&tag)
        {
            report.outputs.push(Output {
                tag,
                tokens: tokens[index],
                preview: message::text(message).chars().take(80).collect(),
            });
        }
    }
    report.tail_tokens = (cut.map_or(0, |(_, end)| end + 1)..messages.len())
        .map(|index| tokens[index])
        .sum();
    let mut newest: Vec<u32> = tags.iter().flatten().copied().collect();
    newest.sort_unstable();
    report.newest_tags = newest.into_iter().rev().take(3).collect();

    if share >= HISTORIAN_TRIGGER {
        report.job = job(messages, &keys, &tags, &tokens, state, inputs, share);
    }
    (out, report)
}

/// The next range to fold: after the last published compartment (else
/// after the first boundary, the session's opening prompt), from a boundary
/// (a user message or displayed custom input) to just before another one
/// ahead of the protected tail.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "token arithmetic on window-sized values"
)]
fn job(
    messages: &[AgentMessage],
    keys: &[Option<String>],
    tags: &[Option<u32>],
    tokens: &[usize],
    state: &ContextState,
    inputs: &Inputs,
    share: f64,
) -> Option<Job> {
    let is_boundary = |index: usize| message::is_boundary(&messages[index]);
    let last_end = state
        .compartments
        .iter()
        .rev()
        .find_map(|c| c.end_key.clone());
    let after = match last_end.as_deref() {
        Some(end_key) => {
            keys.iter()
                .position(|key| key.as_deref() == Some(end_key))?
                + 1
        }
        None => (0..messages.len()).find(|&index| is_boundary(index))? + 1,
    };
    let start = (after..messages.len()).find(|&index| is_boundary(index))?;

    let usable = inputs.window as f64 * EXECUTE;
    let floor = (usable * 0.08).round().clamp(2_000.0, 12_000.0);
    let ceiling = 96_000_f64.min(usable * 0.4).max(floor);
    let tail_target = (usable * 0.3 * (1.0 - share).max(0.0))
        .round()
        .clamp(floor, ceiling) as usize;
    let mut tail = 0;
    let tail_start = (0..messages.len())
        .rev()
        .find(|&index| {
            tail += tokens[index];
            tail >= tail_target
        })
        .unwrap_or(0);

    let mut end_exclusive = (start + 1..=tail_start)
        .rev()
        .find(|&index| is_boundary(index))?;
    loop {
        let range_tokens: usize = tokens[start..end_exclusive].iter().sum();
        if range_tokens <= MAX_RANGE_TOKENS {
            break;
        }
        end_exclusive = (start + 1..end_exclusive)
            .rev()
            .find(|&index| is_boundary(index))?;
    }
    let range_tokens: usize = tokens[start..end_exclusive].iter().sum();
    if end_exclusive - start < MIN_RANGE_MESSAGES || range_tokens < MIN_RANGE_TOKENS {
        return None;
    }
    let end = (start..end_exclusive)
        .rev()
        .find(|&index| keys[index].is_some())?;
    let range_tags: Vec<u32> = tags[start..=end].iter().flatten().copied().collect();
    Some(Job {
        after: last_end,
        start_key: keys[start].clone()?,
        end_key: keys[end].clone()?,
        first_tag: *range_tags.first()?,
        last_tag: *range_tags.last()?,
        tokens: range_tokens,
        transcript: transcript(
            &messages[start..=end],
            &tags[start..=end],
            TRANSCRIPT_ITEM_CHARS,
        ),
    })
}

/// `§N§ U:` / `A:` / `§N§ T:` lines of `messages`, each tool output cut to
/// `item_chars` characters.
pub(crate) fn transcript(
    messages: &[AgentMessage],
    tags: &[Option<u32>],
    item_chars: usize,
) -> String {
    let mut out = String::new();
    for (message, tag) in messages.iter().zip(tags) {
        let (role, cut) = match message::kind(message) {
            Kind::User | Kind::Input => ("U", usize::MAX),
            Kind::Assistant => ("A", usize::MAX),
            Kind::ToolResult => ("T", item_chars),
            Kind::Custom => continue,
        };
        let text = message::text(message);
        let mut body: String = text.chars().take(cut).collect();
        if body.len() < text.len() {
            body.push_str(" [...]");
        }
        if let Some(tag) = tag {
            let _ = write!(out, "§{tag}§ ");
        }
        out.push_str(role);
        out.push_str(": ");
        out.push_str(body.trim());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::tests::{assistant, custom, tool_result, user};
    use crate::state::Compartment;

    /// `turns` user/assistant/tool exchanges, each tool output `size` chars.
    fn conversation(turns: usize, size: usize) -> Vec<AgentMessage> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            let at = i64::try_from(turn).unwrap() * 10;
            let call = format!("c{turn}");
            messages.push(user(&format!("task {turn}"), at));
            messages.push(assistant("working", Some(&call), at + 1));
            messages.push(tool_result(&call, &"o".repeat(size), at + 2));
            messages.push(assistant("done", None, at + 3));
        }
        messages
    }

    fn warm(window: usize) -> Inputs {
        Inputs {
            window,
            cold: false,
        }
    }

    #[test]
    fn tags_render_and_replay_identically() {
        let mut state = ContextState::default();
        let messages = conversation(3, 40);
        let (first, report) = run(&messages, &mut state, &warm(1_000_000));
        assert!(message::text(&first[0]).starts_with("§1§ task 0"));
        assert!(message::text(&first[2]).starts_with("§2§ "));
        assert_eq!(
            message::text(&first[1]),
            message::text(&messages[1]),
            "assistant untouched"
        );
        assert_eq!(report.newest_tags, [6, 5, 4]);
        let (second, _) = run(&messages, &mut state, &warm(1_000_000));
        assert_eq!(
            first, second,
            "a request that changes nothing replays the same bytes"
        );
    }

    #[test]
    fn queued_drops_wait_for_permission() {
        let mut state = ContextState::default();
        let messages = conversation(3, 40);
        run(&messages, &mut state, &warm(1_000_000));
        state.queued.insert(2);
        let (held, report) = run(&messages, &mut state, &warm(1_000_000));
        assert!(!report.permitted);
        assert!(
            message::text(&held[2]).starts_with("§2§ oooo"),
            "a warm cache keeps the bytes"
        );
        let (applied, report) = run(
            &messages,
            &mut state,
            &Inputs {
                window: 1_000_000,
                cold: true,
            },
        );
        assert!(report.permitted);
        assert_eq!(message::text(&applied[2]), "[dropped §2§]");
        assert!(state.queued.is_empty() && state.dropped.contains(&2));
    }

    #[test]
    fn a_dropped_user_message_still_renders() {
        let mut state = ContextState::default();
        let messages = conversation(2, 40);
        run(&messages, &mut state, &warm(1_000_000));
        // An older state dropped the first user message (tag 1).
        state.dropped.insert(1);
        let (out, _) = run(&messages, &mut state, &warm(1_000_000));
        assert_eq!(message::text(&out[0]), "§1§ task 0");
    }

    #[test]
    fn the_force_band_drops_old_outputs_outside_the_protected_tail() {
        let mut state = ContextState::default();
        // 20 outputs of ~2,500 tokens in a 50k window: ~100% usage.
        let messages = conversation(20, 10_000);
        let (out, report) = run(&messages, &mut state, &warm(50_000));
        assert!(!report.emergency_dropped.is_empty());
        assert_eq!(report.emergency_dropped[0], 2, "oldest first");
        assert_eq!(message::text(&out[2]), "[dropped §2§]");
        let last_output = out
            .iter()
            .rev()
            .find(|m| message::kind(m) == Kind::ToolResult)
            .unwrap();
        assert!(
            !message::text(last_output).contains("[dropped"),
            "the tail is protected"
        );
    }

    #[test]
    fn applied_compartments_replace_their_range_with_the_history() {
        let mut state = ContextState::default();
        let messages = conversation(6, 40);
        run(&messages, &mut state, &warm(1_000_000));
        // Fold turns 1-3 (message indexes 4..=15).
        state.compartments.push(Compartment {
            start_key: Some("u:10".to_string()),
            end_key: Some("a:33".to_string()),
            start_tag: 3,
            end_tag: 8,
            title: "middle".to_string(),
            importance: 60,
            created_ms: 0,
            tiers: [
                "did the middle".into(),
                String::new(),
                String::new(),
                String::new(),
            ],
        });
        let (warm_out, _) = run(&messages, &mut state, &warm(1_000_000));
        assert_eq!(warm_out.len(), messages.len(), "published, not yet applied");
        let (out, _) = run(
            &messages,
            &mut state,
            &Inputs {
                window: 1_000_000,
                cold: true,
            },
        );
        assert_eq!(out.len(), messages.len() - 12 + 1);
        let history = message::text(&out[4]);
        assert!(history.starts_with(render::HISTORY_OPEN) && history.contains("did the middle"));
        assert!(message::text(&out[5]).starts_with("§9§ task 4"));
        let (again, _) = run(&messages, &mut state, &warm(1_000_000));
        assert_eq!(out, again, "the history replays byte for byte");
    }

    #[test]
    fn the_historian_range_respects_user_boundaries_and_the_tail() {
        let mut state = ContextState::default();
        // 40 turns of ~1,250-token outputs (~51k) in a 70k window: ~72%.
        let messages = conversation(40, 5_000);
        let (_, report) = run(&messages, &mut state, &warm(70_000));
        let job = report.job.expect("usage past the trigger yields a range");
        assert_eq!(job.start_key, "u:10", "starts after the first user message");
        assert!(
            job.end_key.starts_with("a:"),
            "ends right before a user message"
        );
        assert!(job.tokens <= MAX_RANGE_TOKENS);
        assert!(
            job.transcript
                .starts_with("§3§ U: task 1\nA: working\n[call python_repl] print(1)")
        );
        assert!(job.transcript.contains("§4§ T: "));
        let (_, quiet) = run(&messages, &mut ContextState::default(), &warm(10_000_000));
        assert!(quiet.job.is_none(), "below the trigger nothing folds");
    }

    #[test]
    fn agent_message_turns_are_boundaries_like_user_prompts() {
        // One prompt, then every turn delivered as an agent message (a session
        // driven by `prime-agent send`, a sibling or a parent), with a hidden
        // reminder row in each turn.
        let mut messages = conversation(40, 5_000);
        for turn in 1..40 {
            let at = i64::try_from(turn).unwrap() * 10;
            messages[turn * 4] = custom(&format!("task {turn}"), true, at);
            messages[turn * 4 + 3] = custom("reminder", false, at + 3);
        }
        let (_, report) = run(&messages, &mut ContextState::default(), &warm(70_000));
        let job = report.job.expect("usage past the trigger yields a range");
        assert_eq!(job.start_key, "c:10", "starts at the first agent message");
        assert!(
            job.end_key.starts_with("t:"),
            "ends right before an agent message"
        );
        assert!(job.transcript.starts_with("U: task 1\nA: working\n"));
        assert!(!job.transcript.contains("reminder"), "hidden rows stay out");
    }
}
