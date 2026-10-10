//! The scripted host every executor test runs against: a port of the
//! Python battery's `FakeHost`, clocks, sleeps, and gates.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::factory::executor::ports::{
    FactoryChildren, FactoryClock, FactoryNotices, FactorySpawn, PortFuture,
};
use crate::factory::executor::{
    FactoryExecutor, FactoryExecutorConfig, FactoryRefusal, ResolvedSubagent, RunRequest,
};
use crate::factory::pyvalue::PyValue;
use crate::session_engine::rlm_host::RlmChildResult;

/// An `asyncio.Event`: set once, awaited by any number of tasks.
#[derive(Clone)]
pub struct Event(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Event {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }
}

impl Event {
    pub fn set(&self) {
        self.0.send_replace(true);
    }

    pub async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|set| *set).await;
    }
}

/// The injectable clock (`FakeClock`), starting at 1000.0.
pub struct FakeClock {
    now: Mutex<f64>,
    pub advance_per_collect: Mutex<f64>,
}

impl FakeClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(1000.0),
            advance_per_collect: Mutex::new(0.0),
        })
    }

    pub fn now(&self) -> f64 {
        *self.now.lock().unwrap()
    }

    pub fn advance(&self, seconds: f64) {
        *self.now.lock().unwrap() += seconds;
    }

    pub fn set_advance_per_collect(&self, seconds: f64) {
        *self.advance_per_collect.lock().unwrap() = seconds;
    }
}

/// How the executor's sleeps behave.
#[derive(Clone)]
pub enum SleepMode {
    /// `ClockSleep`: record the delay and advance the fake clock.
    Clock,
    /// `GatedSleep`: record, signal `entered`, and wait for `release`.
    Gated { entered: Event, release: Event },
    /// `turn_sleep`: a real 10ms wait that never advances the clock.
    Turn,
}

/// The executor's clock seam over the fake clock and a sleep mode.
pub struct TestClock {
    pub clock: Arc<FakeClock>,
    pub mode: Mutex<SleepMode>,
    pub sleeps: Mutex<Vec<f64>>,
}

impl FactoryClock for TestClock {
    fn now(&self) -> f64 {
        self.clock.now()
    }

    fn sleep(&self, seconds: f64) -> PortFuture<()> {
        self.sleeps.lock().unwrap().push(seconds);
        let mode = self.mode.lock().unwrap().clone();
        let clock = Arc::clone(&self.clock);
        Box::pin(async move {
            match mode {
                SleepMode::Clock => clock.advance(seconds),
                SleepMode::Gated { entered, release } => {
                    entered.set();
                    release.wait().await;
                }
                SleepMode::Turn => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        })
    }
}

/// One scripted collect outcome.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub status: &'static str,
    pub answer: Option<String>,
    pub error: Option<String>,
}

pub fn done(answer: &str) -> Outcome {
    Outcome {
        status: "done",
        answer: Some(answer.to_string()),
        error: None,
    }
}

pub fn running() -> Outcome {
    Outcome {
        status: "running",
        answer: None,
        error: None,
    }
}

pub fn failed(error: &str) -> Outcome {
    Outcome {
        status: "error",
        answer: None,
        error: Some(error.to_string()),
    }
}

/// A child that exited with an error but whose last assistant text the
/// host captured at the exit (upstream #3462's M4 shape).
pub fn exited(error: &str, answer: &str) -> Outcome {
    Outcome {
        status: "error",
        answer: Some(answer.to_string()),
        error: Some(error.to_string()),
    }
}

/// The host's roster preview cap (`compactRlmText`).
pub const HOST_ANSWER_PREVIEW_CHARS: usize = 160;

/// The factory node a spawn name belongs to: `sw-<node>-<run>-...` for a
/// generated label; a configured inline name spawns run-scoped as
/// `<run6>-<configured>[-i<n>][-a<n>]`, keyed by the configured base (the
/// test machines' configured names never end in those suffix shapes).
pub fn node_of_name(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts[0] == "sw" && parts.len() > 2 {
        return parts[1].to_string();
    }
    let run_prefixed = parts[0].len() == 6 && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit());
    if run_prefixed && parts.len() > 1 {
        let mut base = &parts[1..];
        while base.len() > 1 && is_disambiguation(base[base.len() - 1]) {
            base = &base[..base.len() - 1];
        }
        return base.join("-");
    }
    name.to_string()
}

/// `i<n>` (n >= 1) or `a<n>` (n >= 2): a spawn label's suffix part.
fn is_disambiguation(part: &str) -> bool {
    let mut chars = part.chars();
    let (Some(kind), digits) = (chars.next(), chars.as_str()) else {
        return false;
    };
    let Ok(value) = digits.parse::<u64>() else {
        return false;
    };
    !digits.starts_with('0') && ((kind == 'i' && value >= 1) || (kind == 'a' && value >= 2))
}

#[derive(Default)]
pub struct HostState {
    pub calls: Vec<(String, Value)>,
    pub notices: Vec<Value>,
    pub children: HashMap<String, (String, String)>,
    pub counter: u64,
    pub collects: u64,
    pub advance_per_run: f64,
    pub outcomes: HashMap<String, Outcome>,
    pub child_outcomes: HashMap<String, Outcome>,
    pub rate_limit_first: HashMap<String, usize>,
    pub rate_limit_forever: HashSet<String>,
    gates: HashMap<(String, usize), (Event, Event)>,
    call_indices: HashMap<String, usize>,
    pub delete_fails: bool,
    pub dead_notices: bool,
    /// Children whose FIRST collect settles them with no captured answer
    /// (the host-side capture race); later collects carry the answer.
    pub late_children: HashSet<String>,
    pub collects_of: HashMap<String, u64>,
}

/// Deterministic host fake routing by request type.
pub struct FakeHost {
    pub state: Mutex<HostState>,
    pub clock: Arc<FakeClock>,
}

impl FakeHost {
    pub fn new(clock: Arc<FakeClock>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HostState::default()),
            clock,
        })
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut HostState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn outcome(&self, node: &str, outcome: Outcome) {
        self.with(|state| state.outcomes.insert(node.to_string(), outcome));
    }

    pub fn child_outcome(&self, child: &str, outcome: Outcome) {
        self.with(|state| state.child_outcomes.insert(child.to_string(), outcome));
    }

    /// Suspend the `call_number`-th call of `request_type` until the
    /// returned event is set.
    pub fn gate(&self, request_type: &str, call_number: usize) -> Event {
        let release = Event::default();
        self.with(|state| {
            state.gates.insert(
                (request_type.to_string(), call_number),
                (release.clone(), Event::default()),
            )
        });
        release
    }

    /// The event set when the gated call is entered.
    pub fn gate_entered(&self, request_type: &str, call_number: usize) -> Event {
        self.with(|state| {
            state.gates[&(request_type.to_string(), call_number)]
                .1
                .clone()
        })
    }

    pub fn calls_of(&self, request_type: &str) -> Vec<Value> {
        self.with(|state| {
            state
                .calls
                .iter()
                .filter(|(kind, _)| kind == request_type)
                .map(|(_, payload)| payload.clone())
                .collect()
        })
    }

    pub fn spawn_calls(&self, node: &str) -> Vec<Value> {
        self.calls_of("rlm.run")
            .into_iter()
            .filter(|call| node_of_name(call["kwargs"]["name"].as_str().unwrap()) == node)
            .collect()
    }

    pub fn spawn_prompts(&self, node: &str) -> Vec<String> {
        self.spawn_calls(node)
            .iter()
            .map(|call| call["prompt"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn spawn_names(&self) -> Vec<String> {
        self.calls_of("rlm.run")
            .iter()
            .map(|call| call["kwargs"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn deleted_targets(&self) -> Vec<String> {
        self.calls_of("rlm.delete_subagent")
            .iter()
            .map(|call| call["target"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn notice_kinds(&self) -> Vec<String> {
        self.with(|state| {
            state
                .notices
                .iter()
                .map(|notice| notice["kind"].as_str().unwrap().to_string())
                .collect()
        })
    }

    pub fn collects(&self) -> u64 {
        self.with(|state| state.collects)
    }

    /// Count the call and take its gate, when one is armed.
    fn gate_for(&self, request_type: &str) -> Option<(Event, Event)> {
        self.with(|state| {
            let index = state
                .call_indices
                .entry(request_type.to_string())
                .or_insert(0);
            *index += 1;
            let key = (request_type.to_string(), *index);
            state.gates.remove(&key)
        })
    }

    async fn pass_gate(&self, request_type: &str) {
        if let Some((release, entered)) = self.gate_for(request_type) {
            entered.set();
            release.wait().await;
        }
    }

    fn entry(
        child_id: &str,
        name: &str,
        status: &'static str,
        settled: bool,
        outcome: Option<&Outcome>,
    ) -> RlmChildResult {
        RlmChildResult {
            rlm_child_id: child_id.to_string(),
            session_name: Some(name.to_string()),
            session_dir: Some(format!("/tmp/{child_id}")),
            status,
            settled,
            // The real host hands a compact preview (whitespace-collapsed,
            // capped with an ellipsis tail) plus the FULL final answer as
            // the binding lane.
            answer_preview: outcome
                .and_then(|outcome| outcome.answer.as_deref())
                .map(host_preview),
            error: outcome.and_then(|outcome| outcome.error.clone()),
            duration_ms: Some(5),
            tool_use_count: Some(1),
            replied_since_task: None,
            answer_text: outcome.and_then(|outcome| outcome.answer.clone()),
        }
    }
}

/// The host's roster preview of an answer (`compactRlmText`).
fn host_preview(answer: &str) -> String {
    let compact = answer.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > HOST_ANSWER_PREVIEW_CHARS {
        let kept: String = compact
            .chars()
            .take(HOST_ANSWER_PREVIEW_CHARS - 3)
            .collect();
        format!("{kept}...")
    } else {
        compact
    }
}

/// The host fake's seams, shared by the executor.
pub struct FakePorts(pub Arc<FakeHost>);

impl FactoryChildren for FakePorts {
    fn spawn(&self, request: FactorySpawn) -> PortFuture<Result<String, String>> {
        let host = Arc::clone(&self.0);
        Box::pin(async move {
            host.pass_gate("rlm.run").await;
            let mut kwargs = serde_json::Map::new();
            kwargs.insert("name".into(), Value::from(request.name.clone()));
            if let Some(model) = &request.model {
                kwargs.insert("model".into(), Value::from(model.clone()));
            }
            if let Some(thinking) = &request.thinking {
                kwargs.insert("thinking".into(), Value::from(thinking.clone()));
            }
            let node = node_of_name(&request.name);
            // The supervisor rejects duplicate sibling names: a prior run's
            // settled children stay registered (upstream #3462's M1 class).
            if host.with(|state| {
                state
                    .children
                    .values()
                    .any(|(name, _)| *name == request.name)
            }) {
                return Err(format!(
                    "Agent name \"{}\" is unavailable: an agent of that name already exists at depth 1 under this parent",
                    request.name
                ));
            }
            let advance = host.with(|state| {
                let attempted = state
                    .calls
                    .iter()
                    .filter(|(kind, call)| {
                        kind == "rlm.run"
                            && node_of_name(call["kwargs"]["name"].as_str().unwrap()) == node
                    })
                    .count();
                state.calls.push((
                    "rlm.run".to_string(),
                    json!({ "prompt": request.prompt, "kwargs": kwargs }),
                ));
                let limit = state.rate_limit_first.get(&node).copied().unwrap_or(0)
                    + if state.rate_limit_forever.contains(&node) {
                        999
                    } else {
                        0
                    };
                (state.advance_per_run, attempted < limit)
            });
            host.clock.advance(advance.0);
            if advance.1 {
                return Err("429 rate limit exceeded".to_string());
            }
            Ok(host.with(|state| {
                state.counter += 1;
                let child = format!("child-{}", state.counter);
                state
                    .children
                    .insert(child.clone(), (request.name.clone(), node));
                child
            }))
        })
    }

    fn collect(
        &self,
        child_ids: Vec<String>,
        timeout_ms: u64,
    ) -> PortFuture<Result<Vec<RlmChildResult>, String>> {
        let host = Arc::clone(&self.0);
        Box::pin(async move {
            host.pass_gate("rlm.collect").await;
            let advance = host.with(|state| {
                state.calls.push((
                    "rlm.collect".to_string(),
                    json!({ "targets": child_ids, "timeout_ms": timeout_ms }),
                ));
                state.collects += 1;
                *host.clock.advance_per_collect.lock().unwrap()
            });
            host.clock.advance(advance);
            Ok(host.with(|state| {
                let mut results = Vec::new();
                for target in &child_ids {
                    let Some((name, node)) = state.children.get(target).cloned() else {
                        continue; // deleted children vanish from collect results
                    };
                    let seen = state.collects_of.entry(target.clone()).or_insert(0);
                    *seen += 1;
                    if state.late_children.contains(target) && *seen == 1 {
                        results.push(FakeHost::entry(target, &name, "done", true, None));
                        continue;
                    }
                    let outcome = state
                        .child_outcomes
                        .get(target)
                        .or_else(|| state.outcomes.get(&node))
                        .cloned()
                        .unwrap_or_else(|| done(&format!("answer-{node}")));
                    if outcome.status == "running" {
                        results.push(FakeHost::entry(target, &name, "running", false, None));
                    } else {
                        results.push(FakeHost::entry(
                            target,
                            &name,
                            outcome.status,
                            true,
                            Some(&outcome),
                        ));
                    }
                }
                results
            }))
        })
    }

    fn delete(&self, child_id: String) -> PortFuture<Result<(), String>> {
        let host = Arc::clone(&self.0);
        Box::pin(async move {
            if host.with(|state| state.delete_fails) {
                host.with(|state| {
                    state
                        .calls
                        .push(("rlm.delete_subagent".into(), json!({ "target": child_id })));
                });
                return Err("delete_subagent: child already gone".to_string());
            }
            host.pass_gate("rlm.delete_subagent").await;
            host.with(|state| {
                state
                    .calls
                    .push(("rlm.delete_subagent".into(), json!({ "target": child_id })));
                state.children.remove(&child_id);
            });
            Ok(())
        })
    }
}

impl FactoryNotices for FakePorts {
    fn notify(&self, payload: Value) -> PortFuture<Result<(), String>> {
        let host = Arc::clone(&self.0);
        Box::pin(async move {
            if host.with(|state| state.dead_notices) {
                host.with(|state| state.calls.push(("factory.progress".into(), payload)));
                return Err("bridge dead".to_string());
            }
            host.pass_gate("factory.progress").await;
            host.with(|state| {
                state
                    .calls
                    .push(("factory.progress".into(), payload.clone()));
                state.notices.push(payload);
            });
            Ok(())
        })
    }
}

/// One harness subagent entry (`create_subagent`).
pub struct SubagentRow {
    pub id: String,
    pub title: String,
    pub content: String,
    pub metadata: Value,
}

/// The scripted kernel each test runs against: the fake host, the fake
/// clock and sleeps, a harness of subagents and stored specs, and the
/// executor (the `_ExecutorTestCase` setUp).
pub struct Case {
    pub host: Arc<FakeHost>,
    pub clock: Arc<FakeClock>,
    pub sleeps: Arc<TestClock>,
    pub executor: Arc<FactoryExecutor>,
    pub subagents: Mutex<Vec<SubagentRow>>,
    pub specs: Mutex<HashMap<String, Value>>,
}

impl Case {
    pub fn new() -> Self {
        Self::with_store(None)
    }

    pub fn with_store(store_dir: Option<std::path::PathBuf>) -> Self {
        let clock = FakeClock::new();
        let host = FakeHost::new(Arc::clone(&clock));
        Self::over(host, clock, store_dir)
    }

    /// A case over an existing host and clock (a restarted executor).
    pub fn over(
        host: Arc<FakeHost>,
        clock: Arc<FakeClock>,
        store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let sleeps = Arc::new(TestClock {
            clock: Arc::clone(&clock),
            mode: Mutex::new(SleepMode::Clock),
            sleeps: Mutex::new(Vec::new()),
        });
        let ports = Arc::new(FakePorts(Arc::clone(&host)));
        let executor = Arc::new(FactoryExecutor::new(FactoryExecutorConfig {
            children: ports.clone(),
            notices: ports,
            clock: sleeps.clone(),
            store_dir,
        }));
        let case = Self {
            host,
            clock,
            sleeps,
            executor,
            subagents: Mutex::new(Vec::new()),
            specs: Mutex::new(HashMap::new()),
        };
        case.create_subagent("worker", "Worker", "Do the work carefully.", json!({}));
        case
    }

    pub fn set_sleep(&self, mode: SleepMode) {
        *self.sleeps.mode.lock().unwrap() = mode;
    }

    pub fn sleeps(&self) -> Vec<f64> {
        self.sleeps.sleeps.lock().unwrap().clone()
    }

    pub fn create_subagent(&self, id: &str, title: &str, content: &str, metadata: Value) {
        self.subagents.lock().unwrap().push(SubagentRow {
            id: id.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            metadata,
        });
    }

    /// Store a dag-form spec; write-time validation applies (an invalid
    /// spec panics, like `create_factory` raising).
    pub fn store_factory(&self, dag: Value, spec_id: &str) {
        self.store(dag, spec_id);
    }

    pub fn store_machine(&self, machine: Value, spec_id: &str) {
        self.store(machine, spec_id);
    }

    fn store(&self, spec: Value, spec_id: &str) {
        let errors = crate::factory::spec::validate_factory_spec(&PyValue::from_json(&spec));
        assert_eq!(errors, Vec::<String>::new(), "stored spec must validate");
        self.specs.lock().unwrap().insert(spec_id.to_string(), spec);
    }

    /// Bypass write-time validation (a hand-edited or foreign store).
    pub fn corrupt_stored_spec(&self, spec_id: &str, spec: Value) {
        self.specs.lock().unwrap().insert(spec_id.to_string(), spec);
    }

    /// The kernel client's resolution of a spec's string subagent refs:
    /// the harness entry by id, else the first by title (sorted like
    /// `harness.list`).
    pub fn subagent_table(&self, spec: &Value) -> HashMap<String, Option<ResolvedSubagent>> {
        let states = spec
            .get("states")
            .or_else(|| spec.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rows = self.subagents.lock().unwrap();
        let mut table = HashMap::new();
        for state in states {
            let Some(reference) = state.get("subagent").and_then(Value::as_str) else {
                continue;
            };
            let by_id = rows.iter().find(|row| row.id == reference);
            let mut by_title: Vec<&SubagentRow> =
                rows.iter().filter(|row| row.title == reference).collect();
            by_title.sort_by(|a, b| (&a.title, &a.id).cmp(&(&b.title, &b.id)));
            let row = by_id.or_else(|| by_title.first().copied());
            table.insert(
                reference.to_string(),
                row.map(|row| ResolvedSubagent {
                    content: PyValue::Str(row.content.clone()),
                    model: PyValue::from_json(row.metadata.get("model").unwrap_or(&Value::Null)),
                    thinking: PyValue::from_json(
                        row.metadata.get("thinking").unwrap_or(&Value::Null),
                    ),
                }),
            );
        }
        table
    }

    /// `rlm.factory.run(spec_id)` over the stored specs.
    pub async fn try_start(&self, spec_id: &str) -> Result<Value, FactoryRefusal> {
        let spec = self.specs.lock().unwrap().get(spec_id).cloned();
        let Some(spec) = spec else {
            return Err(FactoryRefusal(format!("unknown factory spec '{spec_id}'")));
        };
        let subagents = self.subagent_table(&spec);
        self.executor
            .run(RunRequest {
                spec_id: spec_id.to_string(),
                name: None,
                spec: PyValue::from_json(&spec),
                subagents,
                library: None,
            })
            .await
    }

    pub async fn start(&self) -> Value {
        self.start_spec("sw").await
    }

    pub async fn start_spec(&self, spec_id: &str) -> Value {
        self.try_start(spec_id).await.expect("run starts")
    }

    pub fn run_id(result: &Value) -> String {
        result["run_id"].as_str().unwrap().to_string()
    }

    pub fn run_state(&self, result: &Value) -> String {
        self.executor
            .snapshot_run(&Self::run_id(result))
            .map(|run| run.state.as_str().to_string())
            .unwrap()
    }

    /// Yield to the control loop until the run leaves `running`, then read
    /// `status()`.
    pub async fn settle(&self, result: &Value) -> Value {
        let run_id = Self::run_id(result);
        for _ in 0..200_000 {
            if self.run_state(result) != "running" {
                return self.status(&run_id);
            }
            tokio::task::yield_now().await;
        }
        panic!("run {run_id} never left the running state");
    }

    pub fn status(&self, run_id: &str) -> Value {
        self.executor.status(run_id).expect("known run")
    }

    pub async fn wait_until(&self, mut predicate: impl FnMut() -> bool) {
        for _ in 0..50_000 {
            if predicate() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition never became true");
    }

    pub fn all_events_of(&self, result: &Value, kind: &str) -> Vec<Value> {
        let run = self.executor.snapshot_run(&Self::run_id(result)).unwrap();
        run.events
            .into_iter()
            .filter(|event| event["kind"] == kind)
            .collect()
    }

    pub fn run_events(&self, result: &Value) -> Vec<Value> {
        self.executor
            .snapshot_run(&Self::run_id(result))
            .unwrap()
            .events
    }

    pub async fn resume(&self, result: &Value) -> Result<Value, FactoryRefusal> {
        self.executor.resume(&Self::run_id(result)).await
    }

    pub async fn stop(&self, result: &Value) -> Value {
        self.executor
            .stop(&Self::run_id(result))
            .await
            .expect("known run")
    }
}

pub fn node_status<'a>(status: &'a Value, node: &str) -> &'a Value {
    status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == node)
        .unwrap_or_else(|| panic!("no node {node}"))
}

pub fn events_of(status: &Value, kind: &str) -> Vec<Value> {
    status["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .cloned()
        .collect()
}

pub fn instance_statuses(node: &Value) -> Vec<String> {
    node["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| instance["status"].as_str().unwrap().to_string())
        .collect()
}

pub fn entry_statuses(node: &Value) -> Vec<String> {
    node["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["status"].as_str().unwrap().to_string())
        .collect()
}

pub fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}
