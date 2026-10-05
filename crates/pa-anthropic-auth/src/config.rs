//! The plugins' sidecar configuration (`anthropic-auth.json`), read the way
//! the pi plugin reads it, so the routing mode, the quota policy and the
//! killswitch a user sets for pi apply to prime-agent too:
//!
//! - the file: `PI_ANTHROPIC_AUTH_FILE`, else `$PI_AGENT_DIR/anthropic-auth.json`,
//!   else `~/.pi/agent/anthropic-auth.json` (the pi plugin's `getPiAccountStoragePath`;
//!   the opencode plugin keeps its own copy under `~/.config/opencode/`);
//! - the sticky routing state beside it (`anthropic-auth-routing-state.json`,
//!   or `PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE`), shared with pi;
//! - re-read when the file changes, so an edit (by a plugin, or by this
//!   crate's commands through `pi/settings.rs`, the same file) applies to
//!   the next request;
//! - what is read: `routing.mode`, `quota.{enabled, checkIntervalMinutes,
//!   refreshEveryNRequests, minimumRemaining, failClosedOnUnknownQuota}` and
//!   `killswitch.{enabled, main, accounts}`, with the plugins' defaults
//!   (`main-first`; quota on, five minutes, no request cadence, no minimum,
//!   fail closed; killswitch off, thresholds 5h 5% / 7d 10% / scoped 0%).
//!   A missing, unreadable or malformed file is the defaults.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anthropic::killswitch::{KillswitchConfig, KillswitchThresholds};
use anthropic::quota::QuotaPolicy;
use anthropic::sticky_routing::RoutingMode;
use pa_types::sync::MutexExt;

/// The sidecar file override (the pi plugin's).
pub const CONFIG_FILE_ENV: &str = "PI_ANTHROPIC_AUTH_FILE";
/// The pi agent directory the default sidecar lives in.
pub const AGENT_DIR_ENV: &str = "PI_AGENT_DIR";
/// The sticky routing state override (the pi plugin's).
pub const ROUTING_STATE_ENV: &str = "PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE";
/// The sidecar's file name.
pub(crate) const CONFIG_FILE_NAME: &str = "anthropic-auth.json";

/// The routing, quota and killswitch settings of the sidecar.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RoutingConfig {
    /// `routing.mode`.
    pub(crate) mode: RoutingMode,
    /// `quota.*`.
    pub(crate) quota: QuotaPolicy,
    /// `killswitch.*`.
    pub(crate) killswitch: KillswitchConfig,
}

/// The sidecar path for an environment lookup and a home directory.
pub(crate) fn config_path_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    home: &Path,
) -> PathBuf {
    let trimmed = |key: &str| {
        lookup(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(file) = trimmed(CONFIG_FILE_ENV) {
        return PathBuf::from(file);
    }
    trimmed(AGENT_DIR_ENV)
        .map_or_else(|| home.join(".pi").join("agent"), PathBuf::from)
        .join(CONFIG_FILE_NAME)
}

/// The sticky routing state for the sidecar at `config_path`
/// (`<stem>-routing-state.json` beside it, or the override).
pub(crate) fn routing_state_path_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    config_path: &Path,
) -> PathBuf {
    lookup(ROUTING_STATE_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || anthropic::sticky_routing::sticky_routing_state_path(config_path),
            PathBuf::from,
        )
}

/// The settings a sidecar document carries (anything but an object is the
/// defaults; a value of the wrong type is its default).
pub(crate) fn parse_config(document: &serde_json::Value) -> RoutingConfig {
    let Some(document) = document.as_object() else {
        return RoutingConfig::default();
    };
    let section = |name: &str| document.get(name).and_then(serde_json::Value::as_object);
    let mode = section("routing")
        .and_then(|routing| routing.get("mode"))
        .and_then(serde_json::Value::as_str)
        .and_then(|mode| match mode {
            "main-first" => Some(RoutingMode::MainFirst),
            "fallback-first" => Some(RoutingMode::FallbackFirst),
            "sticky-balanced" => Some(RoutingMode::StickyBalanced),
            _ => None,
        })
        .unwrap_or_default();
    RoutingConfig {
        mode,
        quota: section("quota").map_or_else(QuotaPolicy::default, parse_quota),
        killswitch: section("killswitch").map_or_else(KillswitchConfig::default, parse_killswitch),
    }
}

fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite())
}

/// `quota.*` (accounts.ts `getQuotaCheckIntervalMs`,
/// `getQuotaRefreshEveryNRequests`, `getQuotaMinimumRemainingThresholds`,
/// `failClosedOnUnknownQuota`, `quotaEnabled`).
fn parse_quota(quota: &serde_json::Map<String, serde_json::Value>) -> QuotaPolicy {
    let defaults = QuotaPolicy::default();
    let minimum = quota
        .get("minimumRemaining")
        .and_then(serde_json::Value::as_object);
    let minimum_for = |name: &str, alias: &str, default: f64| {
        minimum
            .and_then(|minimum| {
                let value = minimum
                    .get(name)
                    .filter(|value| !value.is_null())
                    .or_else(|| minimum.get(alias))?;
                Some(number(Some(value)).unwrap_or(default))
            })
            .unwrap_or(default)
    };
    QuotaPolicy {
        enabled: quota.get("enabled").and_then(serde_json::Value::as_bool) != Some(false),
        check_interval_ms: number(quota.get("checkIntervalMinutes")).map_or(
            defaults.check_interval_ms,
            #[allow(clippy::cast_possible_truncation)]
            // minutes from a JSON number, floored at one by the policy
            |minutes| (minutes.max(1.0) * 60_000.0) as i64,
        ),
        minimum_remaining_five_hour: minimum_for(
            "five_hour",
            "5h",
            defaults.minimum_remaining_five_hour,
        ),
        minimum_remaining_seven_day: minimum_for(
            "seven_day",
            "1w",
            defaults.minimum_remaining_seven_day,
        ),
        fail_closed_on_unknown: quota
            .get("failClosedOnUnknownQuota")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(defaults.fail_closed_on_unknown),
        refresh_every_n_requests: number(quota.get("refreshEveryNRequests"))
            .filter(|every| *every > 0.0)
            .map_or(0, |every| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // a positive request count from a JSON number, floored
                let every = every.floor() as u64;
                every
            }),
    }
}

/// `killswitch.*` (accounts.ts `isKillswitchEnabled`,
/// `normalizeKillswitchThresholds`): only `enabled: true` arms it; a
/// threshold of the wrong type is its default, never its alias.
fn parse_killswitch(killswitch: &serde_json::Map<String, serde_json::Value>) -> KillswitchConfig {
    let thresholds = |value: &serde_json::Value| {
        let value = value.as_object()?;
        // A present non-number keeps the alias out and resolves to the
        // default, as `thresholds.five_hour ?? thresholds['5h']` does.
        let field = |name: &str| {
            value
                .get(name)
                .filter(|value| !value.is_null())
                .map(|value| number(Some(value)).unwrap_or(f64::NAN))
        };
        Some(KillswitchThresholds {
            five_hour: field("five_hour"),
            five_hour_alias: field("5h"),
            seven_day: field("seven_day"),
            seven_day_alias: field("1w"),
            scoped: field("scoped"),
        })
    };
    KillswitchConfig {
        enabled: killswitch
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            == Some(true),
        main: killswitch.get("main").and_then(thresholds),
        accounts: killswitch
            .get("accounts")
            .and_then(serde_json::Value::as_object)
            .map(|accounts| {
                accounts
                    .iter()
                    .filter_map(|(id, value)| Some((id.clone(), thresholds(value)?)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The sidecar file's identity for the memo: `(len, mtime)`, `None` when
/// it does not exist.
type FileStamp = Option<(u64, SystemTime)>;

/// The sidecar, re-read when it changes.
pub(crate) struct ConfigFile {
    /// `None`: no sidecar (tests, sandboxes): the defaults.
    path: Option<PathBuf>,
    memo: Mutex<Option<(FileStamp, Arc<RoutingConfig>)>>,
}

impl ConfigFile {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            memo: Mutex::new(None),
        }
    }

    /// The settings now. Blocking: stats the file, and reads it when it
    /// changed.
    pub(crate) fn current(&self) -> Arc<RoutingConfig> {
        let Some(path) = &self.path else {
            return Arc::new(RoutingConfig::default());
        };
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)));
        let mut memo = self.memo.lock_or_recover();
        if let Some((known, config)) = memo.as_ref() {
            if *known == stamp {
                return Arc::clone(config);
            }
        }
        let config = Arc::new(match stamp {
            None => RoutingConfig::default(),
            Some(_) => read(path),
        });
        *memo = Some((stamp, Arc::clone(&config)));
        config
    }
}

/// The settings in the file at `path`; the defaults when it is unreadable
/// or not JSON (logged without its content).
fn read(path: &Path) -> RoutingConfig {
    let document = std::fs::read(path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|error| error.to_string())
        });
    match document {
        Ok(document) => parse_config(&document),
        Err(error) => {
            tracing::warn!(%error, "the anthropic-auth sidecar is unreadable; using its defaults");
            RoutingConfig::default()
        }
    }
}

#[cfg(test)]
mod tests;
