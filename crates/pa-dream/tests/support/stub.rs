//! A scripted child runner (the TS `makeStub`): it answers by role (read off
//! the prompt header), records every prompt, and tallies calls and the tokens
//! it reports per role. It never spends a real token.

use std::sync::{Mutex, PoisonError};

use pa_dream::child::{RunAgent, RunAgentOptions, RunAgentRequest, RunAgentResult, RunAgentStatus};
use pa_dream::llm::DreamChildRole;

/// One scripted answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub output: String,
    pub status: RunAgentStatus,
    pub tokens: u64,
    pub output_tokens: u64,
    pub stop_reason: Option<&'static str>,
}

impl Answer {
    /// A completed answer with `output` and `tokens`.
    pub fn ok(output: &str, tokens: u64) -> Self {
        Self {
            output: output.to_string(),
            status: RunAgentStatus::Completed,
            tokens,
            output_tokens: 0,
            stop_reason: None,
        }
    }

    /// A non-completed answer.
    pub fn status(status: RunAgentStatus, tokens: u64) -> Self {
        Self {
            output: String::new(),
            status,
            tokens,
            output_tokens: 0,
            stop_reason: None,
        }
    }
}

/// The default guidance-writer insights.
pub const DEFAULT_INSIGHTS: &str =
    "Sets with a wide spread of gaps scored higher; dense arithmetic runs scored lower.";

type Script = Box<dyn Fn(u64) -> Answer + Send + Sync>;

#[derive(Default)]
struct Seen {
    calls: [u64; 3],
    tokens: [u64; 3],
    prompts: [Vec<String>; 3],
    requests: Vec<RunAgentRequest>,
    caps: Vec<Option<u64>>,
}

/// The stub runner.
#[derive(Default)]
pub struct Stub {
    proposer: Option<Script>,
    dreamer: Option<Script>,
    guidance: Option<Script>,
    seen: Mutex<Seen>,
}

fn slot(role: DreamChildRole) -> usize {
    match role {
        DreamChildRole::Proposer => 0,
        DreamChildRole::Dreamer => 1,
        DreamChildRole::Guidance => 2,
    }
}

impl Stub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the proposer by its 1-based call number.
    pub fn proposer(mut self, script: impl Fn(u64) -> Answer + Send + Sync + 'static) -> Self {
        self.proposer = Some(Box::new(script));
        self
    }

    /// Script the dreamer by its 1-based call number.
    pub fn dreamer(mut self, script: impl Fn(u64) -> Answer + Send + Sync + 'static) -> Self {
        self.dreamer = Some(Box::new(script));
        self
    }

    /// Script the guidance writer by its 1-based call number.
    pub fn guidance(mut self, script: impl Fn(u64) -> Answer + Send + Sync + 'static) -> Self {
        self.guidance = Some(Box::new(script));
        self
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls per role.
    pub fn calls(&self, role: DreamChildRole) -> u64 {
        self.seen().calls[slot(role)]
    }

    /// Calls over every role.
    pub fn total_calls(&self) -> u64 {
        self.seen().calls.iter().sum()
    }

    /// Reported tokens over every role.
    pub fn total_tokens(&self) -> u64 {
        self.seen().tokens.iter().sum()
    }

    /// The prompts one role saw, in order.
    pub fn prompts(&self, role: DreamChildRole) -> Vec<String> {
        self.seen().prompts[slot(role)].clone()
    }

    /// Every request, in order.
    pub fn requests(&self) -> Vec<RunAgentRequest> {
        self.seen().requests.clone()
    }

    /// Every call's visible-answer cap, in order.
    pub fn caps(&self) -> Vec<Option<u64>> {
        self.seen().caps.clone()
    }
}

impl RunAgent for Stub {
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        let role = DreamChildRole::of_prompt(&request.prompt)
            .unwrap_or_else(|| panic!("unclassified child prompt: {}", request.prompt));
        let index = slot(role);
        let call = {
            let mut seen = self.seen();
            seen.calls[index] += 1;
            seen.prompts[index].push(request.prompt.clone());
            seen.requests.push(request.clone());
            seen.caps.push(options.max_output_tokens);
            seen.calls[index]
        };
        let script = match role {
            DreamChildRole::Proposer => self.proposer.as_ref(),
            DreamChildRole::Dreamer => self.dreamer.as_ref(),
            DreamChildRole::Guidance => self.guidance.as_ref(),
        };
        let answer = script.map_or_else(
            || {
                if role == DreamChildRole::Guidance {
                    Answer::ok(
                        &serde_json::json!({ "insights": DEFAULT_INSIGHTS }).to_string(),
                        100,
                    )
                } else {
                    Answer::ok("", 100)
                }
            },
            |script| script(call),
        );
        self.seen().tokens[index] += answer.tokens;
        RunAgentResult {
            status: answer.status,
            output: answer.output,
            stop_reason: answer.stop_reason.map(str::to_string),
            total_tokens: answer.tokens,
            output_tokens: answer.output_tokens,
            error: None,
        }
    }
}
