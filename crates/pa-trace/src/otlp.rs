//! Optional OTLP/HTTP JSON export of finished spans plus derived per-name
//! metrics (TS `createOtlpSpanExporter` / `installOtlpExporterFromEnv`).
//!
//! Only created when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Recording a span
//! only pushes it onto a bounded in-memory queue; one background worker owns
//! delivery end to end (batching, the flush interval, request timeouts), so
//! a slow or hanging collector never delays the traced code. Export is
//! diagnostic-only: transport failures are counted and otherwise ignored.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::record::{SpanIds, js_number};

/// The collector base URL; `/v1/traces` and `/v1/metrics` are appended.
pub const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// Optional comma-separated `key=value` request headers.
pub const OTLP_HEADERS_ENV: &str = "OTEL_EXPORTER_OTLP_HEADERS";

const DEFAULT_BATCH_SIZE: usize = 128;
const DEFAULT_MAX_QUEUE_SIZE: usize = 2048;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_MAX_METRIC_SERIES: usize = 256;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTRIBUTE_CHARS: usize = 1024;
const SERVICE_NAME: &str = "prime-agent";
const SCOPE_NAME: &str = "prime-agent";

/// Exporter settings; [`OtlpConfig::from_env`] reads the standard variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpConfig {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub service_version: Option<String>,
    pub batch_size: usize,
    pub max_queue_size: usize,
    pub flush_interval: Duration,
    pub max_metric_series: usize,
    pub request_timeout: Duration,
}

impl OtlpConfig {
    /// The exporter for `endpoint` with the TS defaults.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        OtlpConfig {
            endpoint: endpoint.into(),
            headers: Vec::new(),
            service_version: None,
            batch_size: DEFAULT_BATCH_SIZE,
            max_queue_size: DEFAULT_MAX_QUEUE_SIZE,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            max_metric_series: DEFAULT_MAX_METRIC_SERIES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// `None` unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set to a non-blank value.
    #[must_use]
    pub fn from_env(service_version: &str) -> Option<Self> {
        let endpoint = std::env::var(OTLP_ENDPOINT_ENV).ok()?;
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return None;
        }
        let mut config = OtlpConfig::new(endpoint);
        config.headers = parse_otlp_headers(std::env::var(OTLP_HEADERS_ENV).ok().as_deref());
        config.service_version = Some(service_version.to_string());
        Some(config)
    }
}

/// Parse the comma-separated `key=value` header variable; values are
/// percent-decoded, and a malformed escape keeps the value verbatim.
#[must_use]
pub fn parse_otlp_headers(value: Option<&str>) -> Vec<(String, String)> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for item in value.split(',') {
        let Some(separator) = item.find('=').filter(|index| *index > 0) else {
            continue;
        };
        let key = item[..separator].trim();
        let raw = item[separator + 1..].trim();
        if key.is_empty() {
            continue;
        }
        let decoded = percent_decode(raw).unwrap_or_else(|| raw.to_string());
        match headers.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1 = decoded,
            None => headers.push((key.to_string(), decoded)),
        }
    }
    headers
}

/// `decodeURIComponent`: `None` for a malformed escape or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One finished span handed to the exporter.
#[derive(Debug, Clone)]
pub(crate) struct FinishedSpan {
    pub(crate) name: String,
    pub(crate) ids: SpanIds,
    pub(crate) duration_ms: f64,
    pub(crate) attrs: Map<String, Value>,
    pub(crate) error: Option<String>,
}

struct QueuedSpan {
    span: FinishedSpan,
    end_unix_nano: u128,
}

#[derive(Debug, Clone, PartialEq)]
struct SpanMetric {
    count: u64,
    error_count: u64,
    duration_ms: f64,
}

/// Queue, metrics, and counters shared with the worker.
#[derive(Default)]
struct State {
    queue: VecDeque<QueuedSpan>,
    metrics: Vec<(String, SpanMetric)>,
    dropped: u64,
    export_errors: u64,
    stopped: bool,
    /// Set by the worker once the last batch after a stop is delivered.
    drained: bool,
}

/// Counters for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtlpStats {
    pub queued: usize,
    pub dropped: u64,
    pub export_errors: u64,
}

/// The exporter: a bounded queue plus a lazily started delivery worker.
pub struct OtlpExporter {
    config: OtlpConfig,
    state: Mutex<State>,
    wake: Condvar,
    worker: OnceLock<bool>,
}

impl OtlpExporter {
    pub(crate) fn new(config: OtlpConfig) -> Arc<Self> {
        Arc::new(OtlpExporter {
            config,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            worker: OnceLock::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue one finished span; never waits on delivery. The oldest span is
    /// dropped once the queue is full.
    pub(crate) fn export(self: &Arc<Self>, span: FinishedSpan) {
        self.ensure_worker();
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        record_metric(&mut state.metrics, &span, self.config.max_metric_series);
        if state.queue.len() >= self.config.max_queue_size {
            state.queue.pop_front();
            state.dropped += 1;
        }
        state.queue.push_back(QueuedSpan {
            span,
            end_unix_nano: unix_nanos_now(),
        });
        if state.queue.len() >= self.config.batch_size {
            self.wake.notify_all();
        }
    }

    pub(crate) fn stats(&self) -> OtlpStats {
        let state = self.lock();
        OtlpStats {
            queued: state.queue.len(),
            dropped: state.dropped,
            export_errors: state.export_errors,
        }
    }

    /// Stop accepting spans and wait at most `timeout` for the worker to
    /// deliver what is queued. Answers whether it finished in time; a hanging
    /// collector is abandoned (the process is exiting).
    pub(crate) fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        state.stopped = true;
        self.wake.notify_all();
        if self.worker.get().is_none() {
            return true;
        }
        while !state.drained {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    fn ensure_worker(self: &Arc<Self>) {
        self.worker.get_or_init(|| {
            let exporter = Arc::clone(self);
            std::thread::Builder::new()
                .name("pa-trace-otlp".to_string())
                .spawn(move || exporter.run_worker())
                .is_ok()
        });
    }

    /// The delivery loop: wake on a full batch, the flush interval, or a
    /// stop; deliver every queued batch; after a stop, mark drained.
    fn run_worker(&self) {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let client = reqwest::Client::builder()
            .timeout(self.config.request_timeout)
            .build()
            .unwrap_or_default();
        loop {
            let (batches, stopped) = {
                let mut state = self.lock();
                if !state.stopped && state.queue.len() < self.config.batch_size {
                    state = self
                        .wake
                        .wait_timeout(state, self.config.flush_interval)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
                let mut batches = Vec::new();
                while !state.queue.is_empty() {
                    let take = state.queue.len().min(self.config.batch_size);
                    let batch: Vec<QueuedSpan> = state.queue.drain(..take).collect();
                    let metrics = std::mem::take(&mut state.metrics);
                    batches.push((batch, metrics));
                }
                (batches, state.stopped)
            };
            for (batch, metrics) in batches {
                let failures = runtime.block_on(self.deliver(&client, &batch, &metrics));
                self.lock().export_errors += failures;
            }
            if stopped {
                let mut state = self.lock();
                if state.queue.is_empty() {
                    state.drained = true;
                    self.wake.notify_all();
                    return;
                }
            }
        }
    }

    /// POST one batch's traces and the metrics accumulated with it; answers
    /// the number of failed requests.
    async fn deliver(
        &self,
        client: &reqwest::Client,
        batch: &[QueuedSpan],
        metrics: &[(String, SpanMetric)],
    ) -> u64 {
        let now = unix_nanos_now();
        let longest_ms = batch
            .iter()
            .map(|queued| queued.span.duration_ms)
            .fold(0.0_f64, f64::max);
        let started = now.saturating_sub(millis_to_nanos(longest_ms));
        let trace_body = self.trace_body(batch);
        let metric_body = self.metric_body(metrics, started, now);
        let (traces, metrics) = tokio::join!(
            self.post(client, "traces", &trace_body),
            self.post(client, "metrics", &metric_body)
        );
        u64::from(!traces) + u64::from(!metrics)
    }

    async fn post(&self, client: &reqwest::Client, signal: &str, body: &Value) -> bool {
        let url = format!("{}/v1/{signal}", self.config.endpoint.trim_end_matches('/'));
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string());
        for (key, value) in &self.config.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        matches!(request.send().await, Ok(response) if response.status().is_success())
    }

    fn resource(&self) -> Value {
        let mut attrs = Map::new();
        attrs.insert("service.name".to_string(), Value::from(SERVICE_NAME));
        if let Some(version) = &self.config.service_version {
            attrs.insert("service.version".to_string(), Value::from(version.as_str()));
        }
        json!({ "attributes": attributes(&attrs) })
    }

    fn trace_body(&self, batch: &[QueuedSpan]) -> Value {
        let spans: Vec<Value> = batch
            .iter()
            .map(|queued| {
                let span = &queued.span;
                let mut out = Map::new();
                out.insert(
                    "traceId".to_string(),
                    Value::from(span.ids.context.trace_id_hex()),
                );
                out.insert(
                    "spanId".to_string(),
                    Value::from(span.ids.context.span_id_hex()),
                );
                if let Some(parent) = span.ids.parent_span_id {
                    out.insert(
                        "parentSpanId".to_string(),
                        Value::from(pa_types::trace_context::format_span_id(parent)),
                    );
                }
                out.insert("name".to_string(), Value::from(span.name.as_str()));
                out.insert("kind".to_string(), Value::from(1));
                out.insert(
                    "startTimeUnixNano".to_string(),
                    Value::from(
                        queued
                            .end_unix_nano
                            .saturating_sub(millis_to_nanos(span.duration_ms))
                            .to_string(),
                    ),
                );
                out.insert(
                    "endTimeUnixNano".to_string(),
                    Value::from(queued.end_unix_nano.to_string()),
                );
                out.insert("attributes".to_string(), attributes(&span.attrs));
                let mut status = Map::new();
                status.insert(
                    "code".to_string(),
                    Value::from(if span.error.is_some() { 2 } else { 1 }),
                );
                if let Some(error) = &span.error {
                    status.insert("message".to_string(), Value::from(error.as_str()));
                }
                out.insert("status".to_string(), Value::Object(status));
                Value::Object(out)
            })
            .collect();
        json!({
            "resourceSpans": [{
                "resource": self.resource(),
                "scopeSpans": [{ "scope": { "name": SCOPE_NAME }, "spans": spans }],
            }]
        })
    }

    fn metric_body(&self, metrics: &[(String, SpanMetric)], start: u128, now: u128) -> Value {
        let points = |value: &dyn Fn(&SpanMetric) -> (&'static str, Value)| -> Vec<Value> {
            metrics
                .iter()
                .map(|(name, metric)| {
                    let mut name_attr = Map::new();
                    name_attr.insert("span.name".to_string(), Value::from(name.as_str()));
                    let (key, number) = value(metric);
                    let mut point = Map::new();
                    point.insert("attributes".to_string(), attributes(&name_attr));
                    point.insert(
                        "startTimeUnixNano".to_string(),
                        Value::from(start.to_string()),
                    );
                    point.insert("timeUnixNano".to_string(), Value::from(now.to_string()));
                    point.insert(key.to_string(), number);
                    Value::Object(point)
                })
                .collect()
        };
        let sum = |data_points: Vec<Value>| json!({ "aggregationTemporality": 1, "isMonotonic": true, "dataPoints": data_points });
        json!({
            "resourceMetrics": [{
                "resource": self.resource(),
                "scopeMetrics": [{
                    "scope": { "name": SCOPE_NAME },
                    "metrics": [
                        {
                            "name": "prime_agent.span.count",
                            "sum": sum(points(&|m| ("asInt", Value::from(m.count.to_string())))),
                        },
                        {
                            "name": "prime_agent.span.error_count",
                            "sum": sum(points(&|m| ("asInt", Value::from(m.error_count.to_string())))),
                        },
                        {
                            "name": "prime_agent.span.duration_ms",
                            "sum": sum(points(&|m| ("asDouble", js_number(m.duration_ms)))),
                        },
                    ],
                }],
            }]
        })
    }
}

fn record_metric(metrics: &mut Vec<(String, SpanMetric)>, span: &FinishedSpan, max_series: usize) {
    let index = match metrics.iter().position(|(name, _)| *name == span.name) {
        Some(index) => index,
        None if metrics.len() >= max_series => return,
        None => {
            metrics.push((
                span.name.clone(),
                SpanMetric {
                    count: 0,
                    error_count: 0,
                    duration_ms: 0.0,
                },
            ));
            metrics.len() - 1
        }
    };
    let metric = &mut metrics[index].1;
    metric.count += 1;
    metric.error_count += u64::from(span.error.is_some());
    metric.duration_ms += span.duration_ms;
}

static SENSITIVE_ATTRIBUTE_KEY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:authorization|api[-_.]?key|access[-_.]?token|refresh[-_.]?token|secret|password|cookie)",
    )
    .expect("valid sensitive-key pattern")
});

/// OTLP `KeyValue` list: credential-looking keys are dropped and strings are
/// capped.
fn attributes(values: &Map<String, Value>) -> Value {
    let list: Vec<Value> = values
        .iter()
        .filter(|(key, value)| !value.is_null() && !SENSITIVE_ATTRIBUTE_KEY.is_match(key))
        .map(|(key, value)| json!({ "key": key, "value": any_value(value) }))
        .collect();
    Value::Array(list)
}

fn any_value(value: &Value) -> Value {
    match value {
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && (number.is_i64() || number.is_u64()) => {
                json!({ "intValue": number.to_string() })
            }
            Some(float) if float.fract() == 0.0 => {
                json!({ "intValue": js_number(float).to_string() })
            }
            Some(float) => json!({ "doubleValue": float }),
            None => json!({ "stringValue": number.to_string() }),
        },
        Value::String(text) => {
            json!({ "stringValue": text.chars().take(MAX_ATTRIBUTE_CHARS).collect::<String>() })
        }
        Value::Null | Value::Array(_) | Value::Object(_) => {
            json!({ "stringValue": value.to_string().chars().take(MAX_ATTRIBUTE_CHARS).collect::<String>() })
        }
    }
}

fn unix_nanos_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        * 1_000_000
}

fn millis_to_nanos(ms: f64) -> u128 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "span durations are non-negative and far below u128::MAX nanoseconds"
    )]
    let nanos = (ms.max(0.0) * 1_000_000.0).round() as u128;
    nanos
}

#[cfg(test)]
mod tests;
