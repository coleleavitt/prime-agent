//! The pi plugin's settings file (`anthropic-auth.json` in pi's agent
//! directory: `PI_ANTHROPIC_AUTH_FILE`, else `$PI_AGENT_DIR` or
//! `~/.pi/agent`; anthropic-auth `packages/pi` `paths.ts`), the file its
//! `/claude-fast`, `/claude-cache` and `/claude-cachekeep` commands write
//! and every request reads. prime-agent's TS build loaded the plugin
//! without a `PI_AGENT_DIR` of its own, so both tools share it.
//!
//! Read per request as the plugin does, memoized on the file's size and
//! mtime. A file that does not parse reads as no settings (the plugin
//! fails the request instead; a warning is logged).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use pa_types::sync::MutexExt;
use serde_json::{Map, Value};

use super::convert::{CacheMode, RequestSettings};

/// The settings file's name (`ACCOUNT_FILE_NAME`).
pub(crate) const SETTINGS_FILE: &str = crate::config::CONFIG_FILE_NAME;

/// The settings file pi's plugin uses in this environment: the sidecar
/// the routing reads too (`config.rs`), one file resolved one way.
#[must_use]
pub(crate) fn settings_path_from_env() -> PathBuf {
    crate::config::config_path_from_lookup(
        |name| std::env::var(name).ok(),
        &pa_types::platform::dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    )
}

type Stamp = (u64, SystemTime);

/// The settings file, read on demand.
#[derive(Debug)]
pub(crate) struct PluginSettings {
    path: PathBuf,
    memo: Mutex<Option<(Stamp, Map<String, Value>)>>,
}

impl PluginSettings {
    /// The settings at `path`. No I/O.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            memo: Mutex::new(None),
        }
    }

    /// The configuration object (empty when the file is missing or does
    /// not parse).
    pub(crate) fn read(&self) -> Map<String, Value> {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            *self.memo.lock_or_recover() = None;
            return Map::new();
        };
        let stamp = (
            metadata.len(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        let mut memo = self.memo.lock_or_recover();
        if let Some((seen, value)) = memo.as_ref() {
            if *seen == stamp {
                return value.clone();
            }
        }
        let value = match std::fs::read_to_string(&self.path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())
            }) {
            Ok(Value::Object(map)) => map,
            Ok(_) => Map::new(),
            Err(error) => {
                tracing::warn!(%error, "the pi plugin's settings file does not parse; using defaults");
                Map::new()
            }
        };
        *memo = Some((stamp, value.clone()));
        value
    }

    /// What a request reads: the cache setting and fast mode.
    pub(crate) fn request(&self) -> RequestSettings {
        request_settings(&self.read())
    }
}

fn section<'a>(config: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    config.get(key).and_then(Value::as_object)
}

fn flag(config: &Map<String, Value>, key: &str, field: &str) -> bool {
    section(config, key).and_then(|section| section.get(field)) == Some(&Value::Bool(true))
}

/// The request settings a configuration holds (`isCache1hPersistentlyEnabled`,
/// `getCache1hPersistentMode`, `isFastModePersistentlyEnabled`).
pub(crate) fn request_settings(config: &Map<String, Value>) -> RequestSettings {
    RequestSettings {
        cache_enabled: flag(config, "claudeCache", "enabled"),
        cache_mode: CacheMode::parse(
            section(config, "claudeCache")
                .and_then(|cache| cache.get("mode"))
                .and_then(Value::as_str),
        ),
        fast_mode: flag(config, "claudeFast", "enabled"),
    }
}

/// Why the settings file could not be changed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SettingsError {
    /// The file exists but does not hold JSON (the plugin refuses it too).
    #[error("account store at {path} is corrupt or unreadable ({cause}) — fix or remove it")]
    Corrupt { path: String, cause: String },
    /// Another writer held the plugin's configuration lock past the wait.
    #[error("Timed out waiting for the account configuration write lock")]
    LockTimeout,
    /// Reading, writing or renaming failed.
    #[error("could not write {path}: {cause}")]
    Io { path: String, cause: String },
}

/// The plugin's configuration write lock (`acquireRefreshFileLock`,
/// `config-write`): `<file>.config-write.lock`, created exclusively, holding
/// its owner and expiry.
struct ConfigLock {
    path: PathBuf,
    owner: String,
}

/// The lock's lifetime and how long a writer waits for it (the plugin's
/// `ACCOUNT_CONFIG_LOCK_TTL_MS` / `_WAIT_MS`).
const LOCK_TTL_MS: i64 = 10_000;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(12);
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(25);

impl ConfigLock {
    fn acquire(settings: &std::path::Path) -> Result<Self, SettingsError> {
        let io = |error: std::io::Error| SettingsError::Io {
            path: settings.display().to_string(),
            cause: error.to_string(),
        };
        if let Some(directory) = settings.parent() {
            std::fs::create_dir_all(directory).map_err(io)?;
        }
        let path = PathBuf::from(format!("{}.config-write.lock", settings.display()));
        let owner = uuid::Uuid::new_v4().to_string();
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            let now = chrono::Utc::now().timestamp_millis();
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            match options.open(&path) {
                Ok(mut file) => {
                    use std::io::Write;
                    let record =
                        serde_json::json!({ "ownerId": owner, "expiresAt": now + LOCK_TTL_MS });
                    file.write_all(format!("{record}\n").as_bytes())
                        .map_err(io)?;
                    return Ok(Self { path, owner });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // An expired lock (its owner died) is taken over.
                    let expired = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|owner| owner.get("expiresAt").and_then(Value::as_i64))
                        .is_some_and(|expires| expires <= now);
                    if expired {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                }
                Err(error) => return Err(io(error)),
            }
            if std::time::Instant::now() >= deadline {
                return Err(SettingsError::LockTimeout);
            }
            std::thread::sleep(LOCK_POLL);
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|owner| {
                owner.get("ownerId").and_then(Value::as_str) == Some(self.owner.as_str())
            });
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The top-level keys the plugin writes, in its order (`configFromStorage`).
const CONFIG_ORDER: [&str; 16] = [
    "version",
    "main",
    "routing",
    "fallbackOn",
    "refresh",
    "quota",
    "claudeCache",
    "dump",
    "logging",
    "claudeFast",
    "costZeroing",
    "cacheKeep",
    "relay",
    "killswitch",
    "prime",
    "accounts",
];

/// Keep only `fields` of a section, when it is an object.
fn pick(value: &Value, fields: &[&str]) -> Value {
    let Some(section) = value.as_object() else {
        return Value::Object(Map::new());
    };
    Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                section
                    .get(*field)
                    .map(|value| ((*field).to_string(), value.clone()))
            })
            .collect(),
    )
}

/// The fields the plugin normalizes when it rewrites an existing file
/// (`normalizeStorage` → `configFromStorage`): `version` 1, `main` its
/// provider (and profile), `refresh` / `quota` their known fields (empty
/// when absent), `accounts` (empty when absent). Every other key is kept.
fn normalized(existing: &Map<String, Value>) -> Map<String, Value> {
    let mut normal = Map::new();
    normal.insert("version".to_string(), Value::from(1));
    let mut main = serde_json::json!({ "type": "opencode", "provider": "anthropic" });
    if let Some(profile) = existing.get("main").and_then(|main| main.get("profile")) {
        main["profile"] = profile.clone();
    }
    normal.insert("main".to_string(), main);
    normal.insert(
        "refresh".to_string(),
        pick(
            existing.get("refresh").unwrap_or(&Value::Null),
            &["enabled", "intervalMinutes", "refreshBeforeExpiryMinutes"],
        ),
    );
    normal.insert(
        "quota".to_string(),
        pick(
            existing.get("quota").unwrap_or(&Value::Null),
            &[
                "enabled",
                "checkIntervalMinutes",
                "refreshEveryNRequests",
                "minimumRemaining",
                "failClosedOnUnknownQuota",
                "showToasts",
            ],
        ),
    );
    if !existing.get("accounts").is_some_and(Value::is_array) {
        normal.insert("accounts".to_string(), Value::Array(Vec::new()));
    }
    normal
}

impl PluginSettings {
    /// Change the settings file as the plugin's setters do (`loadAccounts`,
    /// change one section, `saveAccounts`), under its configuration lock:
    /// an existing file keeps its keys in order with the normalized fields
    /// the plugin rewrites ([`normalized`]) and new keys after them in the
    /// plugin's order; a missing one is created as the plugin creates it
    /// (`version`, `main`, the changed section, `accounts`). Written
    /// atomically, owner-only, as `JSON.stringify(config, null, 2)` and a
    /// newline. The plugin's runtime state file is not written.
    pub(crate) fn update(
        &self,
        change: impl FnOnce(&mut Map<String, Value>),
    ) -> Result<(), SettingsError> {
        let _lock = ConfigLock::acquire(&self.path)?;
        let io = |error: std::io::Error| SettingsError::Io {
            path: self.path.display().to_string(),
            cause: error.to_string(),
        };
        let existing = match std::fs::read_to_string(&self.path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(map)) => Some(map),
                Ok(_) => Some(Map::new()),
                Err(error) => {
                    return Err(SettingsError::Corrupt {
                        path: self.path.display().to_string(),
                        cause: error.to_string(),
                    })
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io(error)),
        };
        let config = match existing {
            None => {
                let mut fresh = Map::new();
                fresh.insert("version".to_string(), Value::from(1));
                fresh.insert(
                    "main".to_string(),
                    serde_json::json!({ "type": "opencode", "provider": "anthropic" }),
                );
                change(&mut fresh);
                let mut ordered = Map::new();
                for key in CONFIG_ORDER {
                    if let Some(value) = fresh.remove(key) {
                        ordered.insert(key.to_string(), value);
                    }
                }
                ordered.extend(fresh);
                if !ordered.contains_key("accounts") {
                    ordered.insert("accounts".to_string(), Value::Array(Vec::new()));
                }
                ordered
            }
            Some(existing) => {
                let mut changed = existing.clone();
                change(&mut changed);
                let mut normal = normalized(&existing);
                // `{...existing, ...configFromStorage(storage)}`.
                let mut config = Map::new();
                for (key, value) in &changed {
                    let value = normal.remove(key).unwrap_or_else(|| value.clone());
                    config.insert(key.clone(), value);
                }
                for key in CONFIG_ORDER {
                    if let Some(value) = normal.remove(key) {
                        config.insert(key.to_string(), value);
                    }
                }
                config
            }
        };
        if let Some(directory) = self.path.parent() {
            std::fs::create_dir_all(directory).map_err(io)?;
        }
        let temporary = PathBuf::from(format!(
            "{}.{}.tmp",
            self.path.display(),
            uuid::Uuid::new_v4()
        ));
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(config)).map_err(|error| {
                SettingsError::Io {
                    path: self.path.display().to_string(),
                    cause: error.to_string(),
                }
            })?
        );
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&temporary).and_then(|mut file| {
            use std::io::Write;
            file.write_all(text.as_bytes())
        });
        if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, &self.path)) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(error));
        }
        *self.memo.lock_or_recover() = None;
        Ok(())
    }
}
