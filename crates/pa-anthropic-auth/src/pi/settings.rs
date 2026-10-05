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
pub(crate) const SETTINGS_FILE: &str = "anthropic-auth.json";

/// The settings file pi's plugin uses in this environment.
#[must_use]
pub(crate) fn settings_path_from_env() -> PathBuf {
    settings_path_from_lookup(|name| std::env::var(name).ok())
}

/// [`settings_path_from_env`] over an environment reader.
pub(crate) fn settings_path_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    let non_empty = |name: &str| {
        lookup(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(file) = non_empty("PI_ANTHROPIC_AUTH_FILE") {
        return PathBuf::from(file);
    }
    let directory = non_empty("PI_AGENT_DIR").map_or_else(
        || {
            pa_types::platform::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(".pi")
                .join("agent")
        },
        PathBuf::from,
    );
    directory.join(SETTINGS_FILE)
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
