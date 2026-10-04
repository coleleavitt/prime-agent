//! The durable record of every publish attempt,
//! `<agentDir>/toolforge/ledger.json`: a rejected publish stays rejected with
//! its reason, and a published package stays re-findable by the next process.
//! The file format is the TS product's, byte for byte (two-space JSON plus a
//! trailing newline, camelCase keys); writes are read-modify-write through a
//! temp file and a rename.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

use crate::package::src_path;

/// The ledger schema this crate writes.
pub const LEDGER_SCHEMA: u64 = 1;
/// Oldest records are dropped past this; the ledger is a record, not an archive.
pub const MAX_RECORDS: usize = 500;

/// Which half of the double-run gate a run was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GatePhase {
    /// Against the stub: must raise.
    Negative,
    /// Against the real package: must run clean.
    Positive,
}

impl GatePhase {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Negative => "negative",
            Self::Positive => "positive",
        }
    }
}

/// One half of the double-run gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateRun {
    pub phase: GatePhase,
    /// `raised` / `clean` / `unrunnable`.
    pub outcome: String,
    pub detail: String,
    pub duration_ms: u64,
    /// Whether this run met what its phase requires.
    pub ok: bool,
}

/// How a publish attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishStatus {
    Published,
    Rejected,
}

impl PublishStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Rejected => "rejected",
        }
    }
}

/// One publish attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerRecord {
    pub name: String,
    pub import_name: String,
    pub package_path: String,
    pub source_sha: String,
    pub exit_test_sha: String,
    pub status: PublishStatus,
    /// Why the gate or validation refused it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub gate: Vec<GateRun>,
    /// Whether the editable install into the kernel venv succeeded.
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub at: String,
    /// 1 for the first accepted publish of a name, incrementing on each
    /// republish; 0 on a rejection.
    pub version: u64,
}

/// The whole ledger file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ledger {
    pub schema: u64,
    pub records: Vec<LedgerRecord>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema: LEDGER_SCHEMA,
            records: Vec::new(),
        }
    }
}

/// A package this machine published and still has on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPackage {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub src_path: PathBuf,
    pub version: u64,
}

/// `<agentDir>/toolforge`.
#[must_use]
pub fn toolforge_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("toolforge")
}

/// `<agentDir>/toolforge/ledger.json`.
#[must_use]
pub fn ledger_path(agent_dir: &Path) -> PathBuf {
    toolforge_dir(agent_dir).join("ledger.json")
}

/// The first 16 hex digits of the content's SHA-256.
#[must_use]
pub fn content_sha(content: &str) -> String {
    let digest = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    digest[..16].to_string()
}

fn string_or_empty(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn normalize_gate_run(value: &Value) -> Option<GateRun> {
    let phase = match value.get("phase")?.as_str()? {
        "negative" => GatePhase::Negative,
        "positive" => GatePhase::Positive,
        _ => return None,
    };
    let outcome = value.get("outcome")?.as_str()?.to_string();
    // Integral milliseconds, as the TS product writes them; anything else
    // (a hand-edited fraction or a negative) reads as 0.
    let duration_ms = value.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
    Some(GateRun {
        phase,
        outcome,
        detail: string_or_empty(value.get("detail")),
        duration_ms,
        ok: value.get("ok") == Some(&Value::Bool(true)),
    })
}

fn normalize_record(value: &Value) -> Option<LedgerRecord> {
    let name = non_empty_string(value.get("name"))?;
    let import_name = non_empty_string(value.get("importName"))?;
    let package_path = non_empty_string(value.get("packagePath"))?;
    let status = match value.get("status")?.as_str()? {
        "published" => PublishStatus::Published,
        "rejected" => PublishStatus::Rejected,
        _ => return None,
    };
    let gate = value
        .get("gate")
        .and_then(Value::as_array)
        .map(|runs| runs.iter().filter_map(normalize_gate_run).collect())
        .unwrap_or_default();
    Some(LedgerRecord {
        name,
        import_name,
        package_path,
        source_sha: string_or_empty(value.get("sourceSha")),
        exit_test_sha: string_or_empty(value.get("exitTestSha")),
        status,
        reason: non_empty_string(value.get("reason")),
        gate,
        installed: value.get("installed") == Some(&Value::Bool(true)),
        session_id: non_empty_string(value.get("sessionId")),
        at: value
            .get("at")
            .and_then(Value::as_str)
            .map_or_else(|| "1970-01-01T00:00:00.000Z".to_string(), str::to_string),
        version: value
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version > 0)
            .unwrap_or(1),
    })
}

/// Read a ledger from parsed JSON, dropping records it cannot use.
#[must_use]
pub fn normalize_ledger(value: &Value) -> Ledger {
    let Some(records) = value.get("records").and_then(Value::as_array) else {
        return Ledger::default();
    };
    Ledger {
        schema: LEDGER_SCHEMA,
        records: records.iter().filter_map(normalize_record).collect(),
    }
}

/// Read the ledger. A missing or unparseable file yields an empty ledger
/// (and a warning for the unparseable one), never an error: publishing must
/// not be blocked by a corrupt record of past publishes.
#[must_use]
pub fn load_ledger(path: &Path) -> Ledger {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ledger::default();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) => normalize_ledger(&value),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                bytes = raw.len(),
                error = %error,
                "toolforge.ledger.corrupt"
            );
            Ledger::default()
        }
    }
}

/// Write the ledger through a temp file and a rename.
///
/// # Errors
///
/// The directory, the temp file or the rename could not be written.
pub fn save_ledger(ledger: &Ledger, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let temp = PathBuf::from(temp);
    let body = Ledger {
        schema: LEDGER_SCHEMA,
        records: ledger.records.clone(),
    };
    let written = serde_json::to_string_pretty(&body)
        .map_err(anyhow::Error::from)
        .and_then(|json| Ok(std::fs::write(&temp, format!("{json}\n"))?))
        .and_then(|()| Ok(std::fs::rename(&temp, path)?));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Next version for a name: one past its highest accepted publish.
#[must_use]
pub fn next_version(ledger: &Ledger, name: &str) -> u64 {
    ledger
        .records
        .iter()
        .filter(|record| record.name == name && record.status == PublishStatus::Published)
        .map(|record| record.version)
        .max()
        .unwrap_or(0)
        + 1
}

/// Append one record, dropping the oldest past [`MAX_RECORDS`].
///
/// # Errors
///
/// The ledger could not be written.
pub fn append_record(record: LedgerRecord, path: &Path) -> anyhow::Result<Ledger> {
    let mut ledger = load_ledger(path);
    ledger.records.push(record);
    if ledger.records.len() > MAX_RECORDS {
        let excess = ledger.records.len() - MAX_RECORDS;
        ledger.records.drain(..excess);
    }
    save_ledger(&ledger, path)?;
    Ok(ledger)
}

/// Packages this machine has published and still has on disk, newest
/// accepted record per name. A name whose directory was deleted by hand is
/// dropped, so callers never put a dead root on `sys.path`.
#[must_use]
pub fn published_packages(agent_dir: &Path) -> Vec<PublishedPackage> {
    let ledger = load_ledger(&ledger_path(agent_dir));
    let mut by_name: Vec<PublishedPackage> = Vec::new();
    for record in ledger.records {
        if record.status != PublishStatus::Published {
            continue;
        }
        let package_path = PathBuf::from(&record.package_path);
        let src = src_path(&package_path);
        by_name.retain(|package| package.name != record.name);
        if src.exists() {
            by_name.push(PublishedPackage {
                name: record.name,
                import_name: record.import_name,
                package_path,
                src_path: src,
                version: record.version,
            });
        }
    }
    by_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(name: &str, status: PublishStatus, version: u64) -> LedgerRecord {
        LedgerRecord {
            name: name.to_string(),
            import_name: name.replace('-', "_"),
            package_path: format!("/skills/{name}"),
            source_sha: content_sha("source"),
            exit_test_sha: content_sha("test"),
            status,
            reason: None,
            gate: vec![GateRun {
                phase: GatePhase::Negative,
                outcome: "raised".to_string(),
                detail: "NotImplementedError: x".to_string(),
                duration_ms: 12,
                ok: true,
            }],
            installed: false,
            session_id: None,
            at: "2026-10-03T00:00:00.000Z".to_string(),
            version,
        }
    }

    #[test]
    fn the_file_is_byte_compatible_with_the_ts_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = ledger_path(dir.path());
        let mut first = record("slugify", PublishStatus::Rejected, 0);
        first.reason = Some("nope".to_string());
        first.session_id = Some("s1".to_string());
        append_record(first, &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"schema\": 1,\n  \"records\": [\n    {\n      \"name\": \"slugify\",\n      \"importName\": \"slugify\",\n      \"packagePath\": \"/skills/slugify\",\n      \"sourceSha\": \"41cf6794ba4200b8\",\n      \"exitTestSha\": \"9f86d081884c7d65\",\n      \"status\": \"rejected\",\n      \"reason\": \"nope\",\n      \"gate\": [\n        {\n          \"phase\": \"negative\",\n          \"outcome\": \"raised\",\n          \"detail\": \"NotImplementedError: x\",\n          \"durationMs\": 12,\n          \"ok\": true\n        }\n      ],\n      \"installed\": false,\n      \"sessionId\": \"s1\",\n      \"at\": \"2026-10-03T00:00:00.000Z\",\n      \"version\": 0\n    }\n  ]\n}\n"
        );
    }

    #[test]
    fn loading_drops_unusable_records_and_defaults_the_rest() {
        let ledger = normalize_ledger(&json!({
            "schema": 1,
            "records": [
                { "name": "", "importName": "x", "packagePath": "/p", "status": "published" },
                { "name": "x", "importName": "x", "packagePath": "/p", "status": "pending" },
                {
                    "name": "kept", "importName": "kept", "packagePath": "/p", "status": "published",
                    "gate": [{ "phase": "sideways", "outcome": "clean" }, { "phase": "positive", "outcome": "clean", "ok": true }],
                    "version": -2, "reason": "", "installed": "yes"
                }
            ]
        }));
        assert_eq!(
            ledger,
            Ledger {
                schema: 1,
                records: vec![LedgerRecord {
                    name: "kept".to_string(),
                    import_name: "kept".to_string(),
                    package_path: "/p".to_string(),
                    source_sha: String::new(),
                    exit_test_sha: String::new(),
                    status: PublishStatus::Published,
                    reason: None,
                    gate: vec![GateRun {
                        phase: GatePhase::Positive,
                        outcome: "clean".to_string(),
                        detail: String::new(),
                        duration_ms: 0,
                        ok: true,
                    }],
                    installed: false,
                    session_id: None,
                    at: "1970-01-01T00:00:00.000Z".to_string(),
                    version: 1,
                }],
            }
        );
    }

    #[test]
    fn a_corrupt_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = ledger_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_ledger(&path), Ledger::default());
    }

    #[test]
    fn versions_count_accepted_publishes_only() {
        let ledger = Ledger {
            schema: 1,
            records: vec![
                record("slugify", PublishStatus::Published, 1),
                record("slugify", PublishStatus::Rejected, 0),
                record("slugify", PublishStatus::Published, 2),
                record("other", PublishStatus::Published, 7),
            ],
        };
        assert_eq!(next_version(&ledger, "slugify"), 3);
        assert_eq!(next_version(&ledger, "fresh"), 1);
    }

    #[test]
    fn the_ledger_keeps_the_newest_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = ledger_path(dir.path());
        let mut ledger = Ledger::default();
        // Published records round-trip exactly; a rejected record's stored
        // `version: 0` reads back as 1, as the TS normalizer does.
        for index in 0..MAX_RECORDS {
            ledger
                .records
                .push(record(&format!("n{index}"), PublishStatus::Published, 1));
        }
        save_ledger(&ledger, &path).unwrap();
        let after = append_record(record("last", PublishStatus::Published, 1), &path).unwrap();
        assert_eq!(after.records.len(), MAX_RECORDS);
        assert_eq!(after.records[0].name, "n1");
        assert_eq!(after.records[MAX_RECORDS - 1].name, "last");
        assert_eq!(load_ledger(&path), after);
    }

    #[test]
    fn published_packages_skip_deleted_directories() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        let live = skills.join("live");
        std::fs::create_dir_all(live.join("src")).unwrap();
        let mut records = vec![
            record("live", PublishStatus::Published, 1),
            record("gone", PublishStatus::Published, 1),
            record("live", PublishStatus::Published, 2),
        ];
        records[0].package_path = live.display().to_string();
        records[1].package_path = skills.join("gone").display().to_string();
        records[2].package_path = live.display().to_string();
        save_ledger(&Ledger { schema: 1, records }, &ledger_path(dir.path())).unwrap();
        assert_eq!(
            published_packages(dir.path()),
            vec![PublishedPackage {
                name: "live".to_string(),
                import_name: "live".to_string(),
                package_path: live.clone(),
                src_path: live.join("src"),
                version: 2,
            }]
        );
    }
}
