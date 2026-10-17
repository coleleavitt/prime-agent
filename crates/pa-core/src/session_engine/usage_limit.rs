//! Usage-limit wrap-up: near a subscription's usage limit, ask the model to
//! checkpoint before the limit stops it mid-step.
//!
//! Claude Code 2.1.296 (`hcz0m741:e4o`, gate `tengu_vellum_anchor`, default
//! off) reads the five-hour window's utilization from the
//! `anthropic-ratelimit-unified-5h-*` response headers (`gr4a0fky`, the
//! limits tracker): once it reaches the threshold (`cS` = 0.95) without an
//! overage allowance, a hint arms once per window (keyed by the window's
//! reset), and the next main-loop request inside the tool loop carries the
//! meta row [`NEAR_LIMIT_WRAP_UP_TEXT`].
//!
//! Here the same: [`UsageLimitMonitor::observe`] reads every response's
//! headers through the session's stream hook (Anthropic's unified
//! five-hour window, or the Codex backend's primary window), and the turn
//! boundary after a tool batch steers the hidden row into the running turn
//! ([`UsageLimitMonitor::take_pending`]). Off unless
//! `experimental.usageLimitWrapUp` (or `PRIME_AGENT_USAGE_LIMIT_WRAP_UP`)
//! turns it on. Providers whose responses carry no utilization headers
//! never arm it. Claude Code's grace-window variants (`wrap_up` after the
//! limit, a release note, the subagent braid) depend on server grace
//! headers this harness does not receive and are not ported.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pa_agent::stream::StreamFn;
use pa_types::sync::MutexExt as _;

/// Claude Code's near-limit meta row (`F_s`), verbatim.
pub const NEAR_LIMIT_WRAP_UP_TEXT: &str = "[Usage limit approaching. Checkpoint now: finish the current step, then list up to 3 short bullets of the most impactful remaining work. Don't start subagents or long-running work.]";

/// The custom type of the hidden wrap-up row.
pub const USAGE_LIMIT_WRAP_UP_CUSTOM_TYPE: &str = "usage_limit_wrap_up";

/// One usage window's reading from a response's headers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageReading {
    /// Fraction of the window used (0..=1).
    pub utilization: f64,
    /// When the window resets (Unix seconds), when the provider said.
    pub resets_at: Option<u64>,
    /// The provider would serve past the limit (Anthropic's overage
    /// `allowed` / `allowed_warning`): nothing to wrap up for.
    pub overage_allowed: bool,
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn number(headers: &BTreeMap<String, String>, name: &str) -> Option<f64> {
    header(headers, name)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// The short-window reading a response's headers carry: Anthropic's unified
/// five-hour window (`utilization` is a fraction, `reset` Unix seconds), else
/// the Codex backend's primary window (`used-percent` is 0-100, the reset is
/// `reset-at` Unix seconds or `reset-after-seconds` from `now_secs`).
#[must_use]
pub fn parse_usage_reading(
    headers: &BTreeMap<String, String>,
    now_secs: u64,
) -> Option<UsageReading> {
    if let Some(utilization) = number(headers, "anthropic-ratelimit-unified-5h-utilization") {
        let overage = header(headers, "anthropic-ratelimit-unified-overage-status");
        return Some(UsageReading {
            utilization,
            resets_at: number(headers, "anthropic-ratelimit-unified-5h-reset")
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| seconds as u64),
            overage_allowed: matches!(overage, Some("allowed" | "allowed_warning")),
        });
    }
    let percent = number(headers, "x-codex-primary-used-percent")?;
    let resets_at = number(headers, "x-codex-primary-reset-at")
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| seconds as u64)
        .or_else(|| {
            number(headers, "x-codex-primary-reset-after-seconds")
                .filter(|seconds| *seconds >= 0.0)
                .map(|seconds| now_secs + seconds as u64)
        });
    Some(UsageReading {
        utilization: percent / 100.0,
        resets_at,
        overage_allowed: false,
    })
}

/// The window the hint last armed for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ArmedWindow {
    #[default]
    Never,
    /// The window resetting at this Unix second.
    Resetting(u64),
    /// A window whose reset the provider did not name: arms once per
    /// session.
    Unnamed,
}

impl ArmedWindow {
    fn of(resets_at: Option<u64>) -> Self {
        resets_at.map_or(ArmedWindow::Unnamed, ArmedWindow::Resetting)
    }
}

#[derive(Debug, Default)]
struct MonitorState {
    pending: bool,
    armed_window: ArmedWindow,
}

/// One session's near-limit state.
#[derive(Debug)]
pub struct UsageLimitMonitor {
    threshold: f64,
    state: Mutex<MonitorState>,
}

impl UsageLimitMonitor {
    #[must_use]
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            state: Mutex::new(MonitorState::default()),
        }
    }

    /// Read one response's headers: a reading at or past the threshold arms
    /// the hint once per window; an overage allowance disarms it; a passed
    /// window forgets it (`gr4a0fky` `applyWindowReadings`).
    pub fn observe(&self, headers: &BTreeMap<String, String>, now_secs: u64) {
        let Some(reading) = parse_usage_reading(headers, now_secs) else {
            return;
        };
        let mut state = self.state.lock_or_recover();
        if let ArmedWindow::Resetting(reset) = state.armed_window {
            if now_secs > reset {
                state.pending = false;
                state.armed_window = ArmedWindow::Never;
            }
        }
        if reading.resets_at.is_some_and(|reset| reset <= now_secs) {
            return;
        }
        if reading.utilization < self.threshold {
            return;
        }
        if reading.overage_allowed {
            state.pending = false;
        } else if state.armed_window != ArmedWindow::of(reading.resets_at) {
            state.pending = true;
            state.armed_window = ArmedWindow::of(reading.resets_at);
            tracing::info!(
                target: "pa_core::usage_limit",
                utilization = reading.utilization,
                "usage limit approaching; the next tool-loop request carries the wrap-up note"
            );
        }
    }

    /// Consume the armed hint (`consumePendingNearLimitWrapUpHint`).
    pub fn take_pending(&self) -> bool {
        std::mem::take(&mut self.state.lock_or_recover().pending)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Wrap `stream_fn` so every response's headers reach `monitor` (chained
/// ahead of any response hook already on the request).
#[must_use]
pub fn observe_stream_fn(monitor: Arc<UsageLimitMonitor>, stream_fn: StreamFn) -> StreamFn {
    Arc::new(move |model, context, mut options| {
        let inner = options.on_response.take();
        let monitor = Arc::clone(&monitor);
        options.on_response = Some(Arc::new(move |response, model| {
            monitor.observe(&response.headers, now_secs());
            if let Some(hook) = inner.as_ref() {
                hook(response, model);
            }
        }));
        stream_fn(model, context, options)
    })
}

/// The turn boundary's half: after a tool batch (Claude Code injects only
/// inside the tool loop, `depth > 0`), steer the armed hint into the run as
/// a hidden row.
pub(super) fn after_turn(
    monitor: &UsageLimitMonitor,
    slot: &super::steering_fold::FoldAdmissionSlot,
    turn: &pa_agent::types::ShouldStopAfterTurnContext,
) {
    if turn.tool_results.is_empty() || !monitor.take_pending() {
        return;
    }
    let Some(agent) = slot.get().and_then(|admission| admission.agent.upgrade()) else {
        return;
    };
    super::feature_boundary::steer_hidden_row(
        &agent,
        USAGE_LIMIT_WRAP_UP_CUSTOM_TYPE,
        NEAR_LIMIT_WRAP_UP_TEXT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn readings_parse_from_anthropic_and_codex_headers() {
        assert_eq!(
            parse_usage_reading(
                &headers(&[
                    ("anthropic-ratelimit-unified-5h-utilization", "0.97"),
                    ("anthropic-ratelimit-unified-5h-reset", "2000"),
                    ("anthropic-ratelimit-unified-overage-status", "rejected"),
                ]),
                1000
            ),
            Some(UsageReading {
                utilization: 0.97,
                resets_at: Some(2000),
                overage_allowed: false,
            })
        );
        assert_eq!(
            parse_usage_reading(
                &headers(&[
                    ("X-Codex-Primary-Used-Percent", "96.5"),
                    ("x-codex-primary-reset-after-seconds", "600"),
                ]),
                1000
            ),
            Some(UsageReading {
                utilization: 0.965,
                resets_at: Some(1600),
                overage_allowed: false,
            })
        );
        assert_eq!(
            parse_usage_reading(&headers(&[("retry-after", "3")]), 0),
            None
        );
    }

    #[test]
    fn the_hint_arms_once_per_window_at_the_threshold() {
        let monitor = UsageLimitMonitor::new(0.95);
        let below = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.5"),
            ("anthropic-ratelimit-unified-5h-reset", "5000"),
        ]);
        let near = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.96"),
            ("anthropic-ratelimit-unified-5h-reset", "5000"),
        ]);
        monitor.observe(&below, 1000);
        assert!(!monitor.take_pending());
        monitor.observe(&near, 1000);
        assert!(monitor.take_pending());
        assert!(!monitor.take_pending(), "consumed");
        monitor.observe(&near, 1100);
        assert!(!monitor.take_pending(), "the same window arms once");
        // The window passed; the next window's reading arms again.
        let next = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.99"),
            ("anthropic-ratelimit-unified-5h-reset", "9000"),
        ]);
        monitor.observe(&next, 6000);
        assert!(monitor.take_pending());
    }

    #[test]
    fn an_overage_allowance_never_arms_the_hint() {
        let monitor = UsageLimitMonitor::new(0.95);
        monitor.observe(
            &headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.99"),
                ("anthropic-ratelimit-unified-5h-reset", "5000"),
                ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ]),
            1000,
        );
        assert!(!monitor.take_pending());
    }

    #[tokio::test]
    async fn the_stream_hook_feeds_the_monitor_and_keeps_the_inner_hook() {
        use pa_agent::scripted::ScriptedProvider;
        let model = pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: pa_agent::types::UsageCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            max_tokens_explicit: false,
        };
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        let monitor = Arc::new(UsageLimitMonitor::new(0.95));
        let inner_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A stream fn that reports response headers the way a provider does.
        let reporting: StreamFn = {
            let provider_fn = provider.stream_fn();
            Arc::new(move |model, context, options| {
                if let Some(hook) = options.on_response.as_ref() {
                    hook(
                        pa_agent::stream::ProviderResponse {
                            status: 200,
                            headers: headers(&[
                                ("anthropic-ratelimit-unified-5h-utilization", "0.98"),
                                ("anthropic-ratelimit-unified-5h-reset", "99999999999"),
                            ]),
                        },
                        &model,
                    );
                }
                provider_fn(model, context, options)
            })
        };
        provider.push_text_turn("ok");
        let wrapped = observe_stream_fn(Arc::clone(&monitor), reporting);
        let mut options = pa_agent::stream::StreamRequestOptions::default();
        let seen = Arc::clone(&inner_seen);
        options.on_response = Some(Arc::new(move |_, _| {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        let _stream = wrapped(
            model,
            pa_agent::stream::LlmContext {
                system_prompt: None,
                messages: Vec::new(),
                tools: Vec::new(),
            },
            options,
        )
        .await
        .unwrap();
        assert!(inner_seen.load(std::sync::atomic::Ordering::SeqCst));
        assert!(monitor.take_pending());
    }
}
