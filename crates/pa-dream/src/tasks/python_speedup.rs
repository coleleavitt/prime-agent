//! Python speedup (TS `tasks/python-speedup.ts`): the artifact is a Python 3
//! program summing `|a_i - a_j|` over all pairs from stdin; the root is a
//! correct but deliberately slow reference. `evaluate` runs the candidate under
//! `python3 -I -B` with a PATH-only environment and a strict timeout, checks
//! HIDDEN tests first (any failure scores 0), then scores
//! `baselineTime / candidateTime`, capped. It is wall-clock timed, so this one
//! task's scores are not byte-deterministic; everything else is. This is a
//! trust boundary on what enters the tree, not a security sandbox: the child
//! runs with the user's permissions.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::rng::SeededRng;
use crate::task::{ArtifactShapeError, Evaluation, FailClass, ProposeParams, ScoredTask};

/// The interpreter every candidate runs under.
pub const PYTHON_BIN: &str = "python3";
/// `-I` isolated mode, `-B` no bytecode.
pub const PYTHON_RUN_FLAGS: [&str; 2] = ["-I", "-B"];
/// A candidate source over this many bytes is rejected on parse.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Ceiling on a candidate's stdout.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Strict per-run wall-clock limit.
pub const PER_TEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Timed runs per candidate; the minimum total is taken.
pub const TIMING_REPEATS: usize = 3;
/// The speedup score is clamped to this.
pub const SCORE_CAP: f64 = 1_000.0;

/// The correct but deliberately slow baseline.
pub const REFERENCE_SOLUTION: &str = r#"import sys


def solve(nums):
    n = len(nums)
    reps = 4
    total = 0
    for _ in range(reps):
        total = 0
        for i in range(n):
            ai = nums[i]
            for j in range(i + 1, n):
                d = ai - nums[j]
                total += d if d >= 0 else -d
    return total


def main():
    data = sys.stdin.buffer.read().split()
    if not data:
        return
    n = int(data[0])
    nums = [int(x) for x in data[1 : 1 + n]]
    sys.stdout.write(str(solve(nums)))
    sys.stdout.write("\n")


main()
"#;

/// The public contract an LLM proposer is shown (never the hidden tests).
pub const PYTHON_SPEEDUP_PROMPT_CONTEXT: &str = "Task: rewrite the Python 3 program to run faster while staying correct.
Contract:
- Read all input from standard input. The first integer is n; the next n integers are the array a.
- Write ONE integer to standard output: the sum over every unordered pair (i < j) of abs(a[i] - a[j]).
- Use only the standard library. Do not read files, use the network, or read anything but stdin.
Public examples (stdin -> stdout):
  \"3\\n1 2 3\" -> \"4\"
  \"4\\n-3 7 0 2\" -> \"32\"
Exact output shape: {\"source\": \"<the complete Python 3 program as one JSON string>\"}, a JSON object with exactly this one key.
Return the complete program as the source; it is validated as a single non-empty string.";

/// A candidate program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSpeedupArtifact {
    pub source: String,
}

/// The python-speedup task; the baseline time is measured once per instance.
#[derive(Debug, Default)]
pub struct PythonSpeedup {
    baseline_nanos: OnceLock<Option<f64>>,
}

impl PythonSpeedup {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

struct HiddenTest {
    input: String,
    expected: String,
    timed: bool,
}

fn pairwise_abs_sum(nums: &[i64]) -> i128 {
    let mut sorted = nums.to_vec();
    sorted.sort_unstable();
    let mut total: i128 = 0;
    let mut prefix: i128 = 0;
    for (index, value) in sorted.iter().enumerate() {
        let value = i128::from(*value);
        total += value * i128::try_from(index).unwrap_or(0) - prefix;
        prefix += value;
    }
    total
}

fn hidden(nums: &[i64], timed: bool) -> HiddenTest {
    let joined: Vec<String> = nums.iter().map(ToString::to_string).collect();
    HiddenTest {
        input: format!("{}\n{}\n", nums.len(), joined.join(" ")),
        expected: pairwise_abs_sum(nums).to_string(),
        timed,
    }
}

fn large_input() -> Vec<i64> {
    let mut state: u32 = 0x9e37_79b9;
    (0..700)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            i64::from(state % 1001)
        })
        .collect()
}

fn hidden_tests() -> &'static [HiddenTest] {
    static TESTS: OnceLock<Vec<HiddenTest>> = OnceLock::new();
    TESTS.get_or_init(|| {
        vec![
            hidden(&[1, 2, 3], false),
            hidden(&[5, 5, 5, 5], false),
            hidden(&[-3, 7, 0, 2], false),
            hidden(&[10, -10], false),
            hidden(&[0, 1000, 500, 250, 750, 125], false),
            hidden(&large_input(), true),
        ]
    })
}

enum RunResult {
    Ok { stdout: String, nanos: f64 },
    Timeout,
    Error,
}

/// Run `file` with `input` on stdin under the per-run timeout.
fn run_program(file: &Path, input: &str) -> RunResult {
    let start = Instant::now();
    let deadline = start + PER_TEST_TIMEOUT;
    let mut command = Command::new(PYTHON_BIN);
    command
        .args(PYTHON_RUN_FLAGS)
        .arg(file)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let Ok(mut child) = command.spawn() else {
        return RunResult::Error;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_string();
        std::thread::spawn(move || {
            // A program that exits without reading closes the pipe; that is its business.
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let (sender, receiver) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let limit = u64::try_from(MAX_OUTPUT_BYTES + 1).unwrap_or(u64::MAX);
            let read = (&mut stdout).take(limit).read_to_end(&mut buffer);
            let _ = sender.send(read.map(|_| buffer));
        });
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = match receiver.recv_timeout(remaining) {
        Ok(Ok(buffer)) if buffer.len() <= MAX_OUTPUT_BYTES => buffer,
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return RunResult::Error;
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return RunResult::Timeout;
        }
    };
    // stdout closed: the program has exited or is about to; wait out the rest of the budget.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return RunResult::Timeout;
            }
            Err(_) => return RunResult::Error,
        }
    };
    #[allow(clippy::cast_precision_loss)]
    let nanos = start.elapsed().as_nanos() as f64;
    if !status.success() {
        return RunResult::Error;
    }
    RunResult::Ok {
        stdout: String::from_utf8_lossy(&output).into_owned(),
        nanos,
    }
}

fn check_correctness(file: &Path) -> Option<FailClass> {
    for test in hidden_tests() {
        match run_program(file, &test.input) {
            RunResult::Ok { stdout, .. } => {
                if stdout.trim() != test.expected {
                    return Some(FailClass::Incorrect);
                }
            }
            RunResult::Timeout => return Some(FailClass::Timeout),
            RunResult::Error => return Some(FailClass::RuntimeError),
        }
    }
    None
}

fn time_program(file: &Path) -> Option<f64> {
    let mut best = f64::INFINITY;
    for _ in 0..TIMING_REPEATS {
        let mut total = 0.0;
        for test in hidden_tests().iter().filter(|test| test.timed) {
            match run_program(file, &test.input) {
                RunResult::Ok { nanos, .. } => total += nanos,
                RunResult::Timeout | RunResult::Error => return None,
            }
        }
        if total < best {
            best = total;
        }
    }
    best.is_finite().then_some(best)
}

fn with_source_file<T>(source: &str, run: impl FnOnce(&Path) -> T) -> Option<T> {
    let dir = std::env::temp_dir().join(format!(
        "dream-pyspeed-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let file = dir.join("solution.py");
    let result = std::fs::write(&file, source).ok().map(|()| run(&file));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// The first `reps = <digits>` assignment at a line start: (digits start, digits end, value).
fn find_reps(source: &str) -> Option<(usize, usize, u64)> {
    let bytes = source.as_bytes();
    let mut line_start = 0;
    loop {
        let mut pos = line_start;
        while matches!(bytes.get(pos), Some(b' ' | b'\t')) {
            pos += 1;
        }
        if source[pos..].starts_with("reps") {
            let mut cursor = pos + 4;
            while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'=') {
                cursor += 1;
                while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                    cursor += 1;
                }
                let digits_start = cursor;
                while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                    cursor += 1;
                }
                let boundary = !bytes
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
                if cursor > digits_start && boundary {
                    if let Ok(value) = source[digits_start..cursor].parse() {
                        return Some((digits_start, cursor, value));
                    }
                }
            }
        }
        let next = source[line_start..].find('\n')?;
        line_start += next + 1;
    }
}

/// Lower the redundant `reps` constant: by one, or halved.
fn lower_reps(source: &str, rng: &mut SeededRng) -> String {
    let Some((start, end, current)) = find_reps(source) else {
        return source.to_string();
    };
    if current <= 1 {
        return source.to_string();
    }
    let next = if rng.next_int(2) == 0 {
        current - 1
    } else {
        (current / 2).max(1)
    };
    format!("{}{next}{}", &source[..start], &source[end..])
}

impl ScoredTask for PythonSpeedup {
    type Artifact = PythonSpeedupArtifact;

    fn id(&self) -> &'static str {
        "python-speedup"
    }

    fn root(&self, _rng: &mut SeededRng) -> PythonSpeedupArtifact {
        PythonSpeedupArtifact {
            source: REFERENCE_SOLUTION.to_string(),
        }
    }

    fn propose(
        &self,
        parent: Option<&PythonSpeedupArtifact>,
        _params: &ProposeParams,
        rng: &mut SeededRng,
        _round: u32,
    ) -> PythonSpeedupArtifact {
        let base = parent.map_or(REFERENCE_SOLUTION, |parent| parent.source.as_str());
        if rng.next_int(3) == 0 {
            return PythonSpeedupArtifact {
                source: base.to_string(),
            };
        }
        PythonSpeedupArtifact {
            source: lower_reps(base, rng),
        }
    }

    fn evaluate(&self, candidate: &PythonSpeedupArtifact) -> Evaluation {
        if candidate.source.is_empty() {
            return Evaluation::invalid(FailClass::RuntimeError);
        }
        with_source_file(&candidate.source, |file| {
            if let Some(fail_class) = check_correctness(file) {
                return Evaluation::invalid(fail_class);
            }
            let baseline = self
                .baseline_nanos
                .get_or_init(|| with_source_file(REFERENCE_SOLUTION, time_program).flatten());
            let Some(baseline) = *baseline else {
                return Evaluation::invalid(FailClass::RuntimeError);
            };
            let Some(candidate_nanos) = time_program(file) else {
                return Evaluation::invalid(FailClass::RuntimeError);
            };
            let ratio = baseline / candidate_nanos.max(1.0);
            Evaluation::valid(ratio.clamp(0.0, SCORE_CAP))
        })
        .unwrap_or(Evaluation::invalid(FailClass::RuntimeError))
    }

    fn serialize(&self, candidate: &PythonSpeedupArtifact) -> Value {
        json!({ "source": candidate.source })
    }

    fn deserialize(&self, value: &Value) -> Result<PythonSpeedupArtifact, ArtifactShapeError> {
        let Value::Object(record) = value else {
            return Err(ArtifactShapeError(
                "python-speedup artifact must be an object".to_string(),
            ));
        };
        let Some(source) = record
            .get("source")
            .and_then(Value::as_str)
            .filter(|source| !source.is_empty())
        else {
            return Err(ArtifactShapeError(
                "python-speedup artifact must have a non-empty source string".to_string(),
            ));
        };
        if source.len() > MAX_SOURCE_BYTES {
            return Err(ArtifactShapeError(format!(
                "python-speedup source exceeds {MAX_SOURCE_BYTES} bytes"
            )));
        }
        Ok(PythonSpeedupArtifact {
            source: source.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Seed;

    #[test]
    fn the_hidden_answers_are_the_pairwise_sums() {
        assert_eq!(pairwise_abs_sum(&[1, 2, 3]), 4);
        assert_eq!(pairwise_abs_sum(&[-3, 7, 0, 2]), 32);
        assert_eq!(hidden(&[1, 2, 3], false).input, "3\n1 2 3\n");
        assert_eq!(large_input().len(), 700);
    }

    #[test]
    fn the_local_proposer_only_lowers_reps_and_never_below_one() {
        let task = PythonSpeedup::new();
        let mut rng = SeededRng::new(&Seed::Number(1));
        let params = ProposeParams {
            step_scale: 0.2,
            refine_depth: 2,
            branch_width: 2,
        };
        let mut current = task.root(&mut rng);
        for round in 0..20 {
            current = task.propose(Some(&current), &params, &mut rng, round);
        }
        let reps = find_reps(&current.source).map(|(_, _, value)| value);
        assert_eq!(reps, Some(1));
        assert_eq!(
            current.source.replace("reps = 1", "reps = 4"),
            REFERENCE_SOLUTION
        );
        assert!(task.deserialize(&json!({"source": ""})).is_err());
    }
}
