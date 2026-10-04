//! Replay cases: executable reproductions a failure record carries (the
//! data half of TS `ravo/referee.ts`).
//!
//! A case is derived only from the kernel's own traceback for an `ipython`
//! cell that raised, and only for the environment failures a skill write can
//! fix: a missing module (`import x`) and a missing distribution
//! (`importlib.metadata.version("d")`). A stored case whose source is not
//! exactly a valid rendered probe is dropped wherever a case list is built.
//! Running a case and adjudicating a claim with it belongs to the referee
//! (`pa-ravo`); this module only stores, derives, and validates.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fingerprint::{FailureFingerprint, FailureKind};
use crate::js::{js_len, js_trim, json_string, JS_WHITESPACE_CLASS};

/// Distinct cases a record keeps per fingerprint; the oldest is evicted first.
pub const MAX_REPLAY_CASES: usize = 8;
/// Longest case source kept (UTF-16 units).
pub const MAX_REPLAY_SOURCE_CHARS: usize = 600;

/// Top-level modules whose import alone acts (a browser, a GUI, a CLI, pip,
/// a debugger, a server); never replayed.
pub const REPLAY_MODULE_DENYLIST: [&str; 31] = [
    "antigravity",
    "this",
    "idlelib",
    "turtledemo",
    "turtle",
    "tkinter",
    "webbrowser",
    "venv",
    "ensurepip",
    "pip",
    "pydoc",
    "zipapp",
    "site",
    "sitecustomize",
    "usercustomize",
    "runpy",
    "code",
    "pdb",
    "cProfile",
    "profile",
    "trace",
    "timeit",
    "doctest",
    "unittest",
    "http",
    "xmlrpc",
    "smtpd",
    "ftplib",
    "telnetlib",
    "socketserver",
    "wsgiref",
];

/// An executable reproduction of a recorded failure. Field order is the TS
/// object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCase {
    /// Always `"python"`.
    pub language: String,
    /// Program that must raise the recorded exception while the failure is live.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<String>,
    /// Extra `sys.path` roots the case needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys_path: Option<Vec<String>>,
    /// When the self-check saw this case reproduce the recorded failure; a
    /// case that never reproduced is not evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
}

/// The side-effect-free probes a replay case may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayProbe {
    /// `import X`: a missing module.
    Module(String),
    /// `importlib.metadata.version("d")`: a missing distribution.
    Distribution(String),
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("invalid built-in pattern {pattern}: {error}"))
}

static MODULE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*$"));
static DISTRIBUTION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[A-Za-z0-9][A-Za-z0-9._-]*$"));
static NO_MODULE_NAMED: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)no module named ['"]([^'"]+)['"]"#));
// `$` under the TS `m` flag also matches before `\r`, U+2028 and U+2029;
// consuming the terminator instead changes neither the match nor the capture.
static NO_PACKAGE_METADATA: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)no package metadata was found for ['"]?([^{JS_WHITESPACE_CLASS}'"]+?)['"]?[{JS_WHITESPACE_CLASS}]*(?:(?m:$)|[\r\x{{2028}}\x{{2029}}])"#
    ))
});
static PACKAGE_NOT_FOUND_LINE: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"PackageNotFoundError: ['"]?([^{JS_WHITESPACE_CLASS}'"]+?)['"]?[{JS_WHITESPACE_CLASS}]*(?:(?m:$)|[\r\x{{2028}}\x{{2029}}])"#
    ))
});
static MODULE_PROBE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^import ([^{JS_WHITESPACE_CLASS}]+)$")));
static DISTRIBUTION_PROBE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"^import importlib\.metadata\nimportlib\.metadata\.version\("([^"\\]*)"\)$"#)
});

/// A module path a replay may import: no private segment, no denylisted top level.
#[must_use]
pub fn is_replayable_module_path(module: &str) -> bool {
    if !MODULE_PATTERN.is_match(module) {
        return false;
    }
    let mut segments = module.split('.');
    let top = segments.next().unwrap_or_default();
    !REPLAY_MODULE_DENYLIST.contains(&top)
        && module.split('.').all(|segment| !segment.starts_with('_'))
}

fn valid_probe(probe: &ReplayProbe) -> bool {
    match probe {
        ReplayProbe::Module(module) => is_replayable_module_path(module),
        ReplayProbe::Distribution(distribution) => DISTRIBUTION_PATTERN.is_match(distribution),
    }
}

/// Render a probe as Python. String operands are JSON literals, which are
/// valid Python string literals.
#[must_use]
pub fn replay_probe_source(probe: &ReplayProbe) -> String {
    match probe {
        ReplayProbe::Module(module) => format!("import {module}"),
        ReplayProbe::Distribution(distribution) => format!(
            "import importlib.metadata\nimportlib.metadata.version({})",
            json_string(distribution)
        ),
    }
}

/// The probe a stored case runs, or `None` when its source is not exactly a
/// valid rendered probe.
#[must_use]
pub fn replay_probe_of(source: &str) -> Option<ReplayProbe> {
    let parsed = [
        MODULE_PROBE
            .captures(source)
            .map(|captures| ReplayProbe::Module(captures[1].to_string())),
        DISTRIBUTION_PROBE
            .captures(source)
            .map(|captures| ReplayProbe::Distribution(captures[1].to_string())),
    ];
    parsed
        .into_iter()
        .flatten()
        .find(|probe| valid_probe(probe) && replay_probe_source(probe) == source)
}

fn bare_class(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Derive an executable reproduction from the kernel's own traceback
/// excerpt. Only a missing module or a missing distribution qualifies.
#[must_use]
pub fn derive_replay_case(fingerprint: &FailureFingerprint, excerpt: &str) -> Option<ReplayCase> {
    if fingerprint.kind != FailureKind::PythonException {
        return None;
    }
    let (probe, exception_class) = if let Some(missing) = NO_MODULE_NAMED.captures(excerpt) {
        (ReplayProbe::Module(missing[1].to_string()), None)
    } else {
        let class = fingerprint.exception_class.as_deref().map(bare_class);
        let distribution = NO_PACKAGE_METADATA
            .captures(excerpt)
            .map(|captures| captures[1].to_string())
            .or_else(|| {
                (class == Some("PackageNotFoundError"))
                    .then(|| PACKAGE_NOT_FOUND_LINE.captures(excerpt))
                    .flatten()
                    .map(|captures| captures[1].to_string())
            })?;
        (
            ReplayProbe::Distribution(distribution),
            Some("PackageNotFoundError".to_string()),
        )
    };
    if !valid_probe(&probe) {
        return None;
    }
    let source = replay_probe_source(&probe);
    if js_len(&source) > MAX_REPLAY_SOURCE_CHARS {
        return None;
    }
    Some(ReplayCase {
        language: "python".to_string(),
        source,
        exception_class: exception_class.or_else(|| fingerprint.exception_class.clone()),
        sys_path: None,
        verified_at: None,
    })
}

/// Read one stored case leniently (TS `normalizeReplayCase`).
#[must_use]
pub fn normalize_replay_case(value: &Value) -> Option<ReplayCase> {
    let raw = value.as_object()?;
    if raw.get("language").and_then(Value::as_str) != Some("python") {
        return None;
    }
    let source = raw.get("source").and_then(Value::as_str)?;
    if js_trim(source).is_empty() || js_len(source) > MAX_REPLAY_SOURCE_CHARS {
        return None;
    }
    let sys_path: Vec<String> = raw
        .get("sysPath")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(ReplayCase {
        language: "python".to_string(),
        source: source.to_string(),
        exception_class: raw
            .get("exceptionClass")
            .and_then(Value::as_str)
            .map(str::to_string),
        sys_path: (!sys_path.is_empty()).then_some(sys_path),
        verified_at: raw
            .get("verifiedAt")
            .and_then(Value::as_str)
            .filter(|at| !at.is_empty())
            .map(str::to_string),
    })
}

/// Normalize a stored case list; a legacy single `replayCase` becomes the
/// list's oldest entry and a case that is not a valid probe is dropped
/// before the bound applies.
#[must_use]
pub fn normalize_replay_cases(value: Option<&Value>, legacy: Option<&Value>) -> Vec<ReplayCase> {
    let listed = value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut cases = Vec::new();
    for item in legacy.into_iter().chain(listed) {
        if let Some(replay) = normalize_replay_case(item) {
            if replay_probe_of(&replay.source).is_some() {
                cases = fold_replay_probe(&cases, replay);
            }
        }
    }
    cases
}

/// Fold one case into a list kept distinct by source, oldest first, bounded
/// to [`MAX_REPLAY_CASES`], keeping only valid probes. A re-observed source
/// moves to the newest slot and never loses an earlier `verifiedAt`.
#[must_use]
pub fn merge_replay_case(cases: &[ReplayCase], incoming: ReplayCase) -> Vec<ReplayCase> {
    let live: Vec<ReplayCase> = cases
        .iter()
        .filter(|replay| replay_probe_of(&replay.source).is_some())
        .cloned()
        .collect();
    if replay_probe_of(&incoming.source).is_none() {
        return live;
    }
    fold_replay_probe(&live, incoming)
}

fn fold_replay_probe(probes: &[ReplayCase], incoming: ReplayCase) -> Vec<ReplayCase> {
    let existing = probes
        .iter()
        .find(|replay| replay.source == incoming.source);
    let merged = match existing {
        Some(ReplayCase {
            verified_at: Some(verified_at),
            ..
        }) if incoming.verified_at.is_none() => {
            // `{ ...incoming, verifiedAt }`: the stamp moves to the end, which
            // is where the struct keeps it anyway.
            ReplayCase {
                verified_at: Some(verified_at.clone()),
                ..incoming
            }
        }
        _ => incoming,
    };
    let mut next: Vec<ReplayCase> = probes
        .iter()
        .filter(|replay| replay.source != merged.source)
        .cloned()
        .collect();
    next.push(merged);
    if next.len() > MAX_REPLAY_CASES {
        next.drain(..next.len() - MAX_REPLAY_CASES);
    }
    next
}

/// The cases that reproduced their recorded failure and are valid probes:
/// a fingerprint's replay evidence, oldest first.
#[must_use]
pub fn verified_replay_cases(cases: &[ReplayCase]) -> Vec<&ReplayCase> {
    cases
        .iter()
        .filter(|replay| replay.verified_at.is_some() && replay_probe_of(&replay.source).is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_failure;

    fn case(source: &str, verified_at: Option<&str>) -> ReplayCase {
        ReplayCase {
            language: "python".to_string(),
            source: source.to_string(),
            exception_class: Some("ModuleNotFoundError".to_string()),
            sys_path: None,
            verified_at: verified_at.map(str::to_string),
        }
    }

    #[test]
    fn derives_a_module_and_a_distribution_probe_and_nothing_else() {
        let module = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("ModuleNotFoundError"),
            "No module named 'polars'",
        );
        assert_eq!(
            derive_replay_case(&module, "ModuleNotFoundError: No module named 'polars'"),
            Some(case("import polars", None))
        );
        let distribution = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("importlib.metadata.PackageNotFoundError"),
            "No package metadata was found for foo-bar",
        );
        assert_eq!(
            derive_replay_case(
                &distribution,
                "importlib.metadata.PackageNotFoundError: No package metadata was found for foo-bar"
            ),
            Some(ReplayCase {
                language: "python".to_string(),
                source: "import importlib.metadata\nimportlib.metadata.version(\"foo-bar\")"
                    .to_string(),
                exception_class: Some("PackageNotFoundError".to_string()),
                sys_path: None,
                verified_at: None,
            })
        );
        let attribute = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("AttributeError"),
            "x",
        );
        assert_eq!(
            derive_replay_case(
                &attribute,
                "AttributeError: module 'a' has no attribute 'b'"
            ),
            None
        );
        assert_eq!(derive_replay_case(&module, "No module named 'pip'"), None);
        assert_eq!(
            derive_replay_case(&module, "No module named '_private'"),
            None
        );
    }

    #[test]
    fn only_exact_renders_of_valid_probes_parse() {
        assert_eq!(
            replay_probe_of("import polars"),
            Some(ReplayProbe::Module("polars".to_string()))
        );
        assert_eq!(replay_probe_of("import polars "), None);
        assert_eq!(replay_probe_of("import os; os.system('x')"), None);
        assert_eq!(replay_probe_of("import tkinter"), None);
        assert_eq!(replay_probe_of("from a import b"), None);
    }

    #[test]
    fn merging_keeps_sources_distinct_newest_last_bounded_and_verified() {
        let mut cases = vec![case("import a", Some("t0"))];
        for name in ["b", "c", "d", "e", "f", "g", "h", "i"] {
            cases = merge_replay_case(&cases, case(&format!("import {name}"), None));
        }
        assert_eq!(cases.len(), MAX_REPLAY_CASES);
        assert_eq!(cases[0].source, "import b");
        let cases = merge_replay_case(&cases, case("import b", None));
        assert_eq!(cases.last().unwrap().source, "import b");
        let verified = merge_replay_case(&[case("import x", Some("t1"))], case("import x", None));
        assert_eq!(verified, vec![case("import x", Some("t1"))]);
        assert_eq!(
            merge_replay_case(&[case("import os; x", None)], case("import tkinter", None)),
            Vec::<ReplayCase>::new()
        );
    }
}
