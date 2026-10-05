//! The plugins' cache keep-alive (anthropic-auth core `cachekeep.ts` and
//! `cachekeep-registry.ts`, as pi's `stream.ts` runs them, with the
//! plugin's timing): a store-served request whose prompt cache is kept
//! (`/claude-cache mode hybrid`, and `/claude-cachekeep always` or a local
//! `HH-HH` window) is remembered per session; five minutes before its
//! one-hour cache would expire, the same request is sent again with
//! `max_tokens: 0` (no output; the cache is read and its TTL renewed), and
//! again an hour after each success. A failure is retried with the plugin's
//! jittered backoff (one minute doubling to fifteen) while the cache the
//! last success created still lives; once it expired, a prewarm would be a
//! paid cold write, so the session is dropped.
//!
//! Where prime-agent differs from the plugin:
//!
//! - the prewarm authenticates with the store's current token for the
//!   login the session's request was served with (refreshed under the
//!   store's claim when expired), never a token it remembered;
//! - the ticks run on the crate's own thread (`anthropic-cachekeep`,
//!   started by the first tracked request; it parks while nothing is
//!   tracked), never on a request, paint or startup path;
//! - a sticky session's next assignment prefers the login its cache is
//!   kept warm on (the opencode plugin's `trackedOAuthRoute`;
//!   `routing.rs`).
//!
//! The registry (`<tmp>/opencode-anthropic-auth/cachekeep-sessions/pi`, or
//! `PI_ANTHROPIC_AUTH_CACHEKEEP_REGISTRY_DIR`, shared with pi): one
//! `<pid>-<uuid>.json` per process listing its tracked sessions, rewritten
//! after every change and tick (a three-minute lease), so
//! `/claude-cachekeep` lists every live process's sessions.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, FixedOffset, Timelike};
use pa_types::sync::MutexExt;
use serde_json::{Map, Value};

pub(crate) mod prewarm;

/// `/claude-cachekeep`.
pub(crate) const COMMAND: &str = "claude-cachekeep";
pub(crate) const DESCRIPTION: &str =
    "Keep hybrid Claude cache warm always or during a local time window";
pub(crate) const HINT: &str = "[always|off|HH-HH|subagents on|off]";

/// How long a written cache lives (`CACHE_KEEP_TTL_MS`).
pub(crate) const TTL_MS: i64 = 60 * 60_000;
/// How long before it expires a cache is renewed (`CACHE_KEEP_PREWARM_LEAD_MS`).
pub(crate) const PREWARM_LEAD_MS: i64 = 5 * 60_000;
/// The scheduler's tick (`CACHE_KEEP_TICK_MS`).
pub(crate) const TICK: std::time::Duration = std::time::Duration::from_mins(1);
/// The beta a prewarm adds (`CACHE_KEEP_EXTENDED_TTL_BETA`).
pub(crate) const EXTENDED_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
/// The most sessions kept (`CACHE_KEEP_MAX_TARGETS`).
pub(crate) const MAX_TARGETS: usize = 32;
/// The most request text kept, in UTF-16 units (`CACHE_KEEP_MAX_BODY_BYTES`,
/// counted as JavaScript counts a string's length).
pub(crate) const MAX_BODY_UNITS: usize = 16 * 1024 * 1024;
/// A prewarm's bound (`CACHE_KEEP_PREWARM_TIMEOUT_MS`).
pub(crate) const PREWARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const RETRY_BASE_MS: i64 = 60_000;
const RETRY_MAX_MS: i64 = 15 * 60_000;
const RETRY_JITTER_MAX_MS: i64 = 30_000;
/// A registry record older than this belongs to a dead process
/// (`CACHE_KEEP_REGISTRY_LEASE_MS`).
const REGISTRY_LEASE_MS: i64 = 3 * 60_000;
/// The registry directory override (pi's).
pub(crate) const REGISTRY_DIR_ENV: &str = "PI_ANTHROPIC_AUTH_CACHEKEEP_REGISTRY_DIR";

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// JavaScript's `Number(value)` for a JSON value (`undefined`: `None`).
fn js_number_of(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Null) => 0.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                0.0
            } else {
                text.parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
                    .unwrap_or(f64::NAN)
            }
        }
        Some(Value::Array(items)) if items.is_empty() => 0.0,
        None | Some(Value::Array(_) | Value::Object(_)) => f64::NAN,
    }
}

/// A local-time keep window, `start` to `end` hour (overnight when
/// `start > end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) start_hour: u32,
    pub(crate) end_hour: u32,
}

impl Window {
    /// `isWithinCacheKeepWindow`.
    fn contains(self, hour: u32) -> bool {
        if self.start_hour < self.end_hour {
            hour >= self.start_hour && hour < self.end_hour
        } else {
            hour >= self.start_hour || hour < self.end_hour
        }
    }
}

/// The keep-alive's settings in the plugin's settings file (`cacheKeep`,
/// and `claudeCache`'s mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Settings {
    /// `cacheKeep.enabled === true`.
    pub(crate) enabled: bool,
    /// `cacheKeep.always === true`.
    pub(crate) always: bool,
    /// `getCacheKeepWindow`.
    pub(crate) window: Option<Window>,
    /// `claudeCache.enabled === true && claudeCache.mode === 'hybrid'`.
    pub(crate) hybrid_cache: bool,
}

impl Settings {
    /// The settings `config` holds.
    pub(crate) fn from_config(config: &Map<String, Value>) -> Self {
        let section = |name: &str| config.get(name).and_then(Value::as_object);
        let keep = section("cacheKeep");
        let flag = |name: &str| keep.and_then(|keep| keep.get(name)) == Some(&Value::Bool(true));
        let hour = |name: &str| {
            let number = js_number_of(keep.and_then(|keep| keep.get(name)));
            (number.fract() == 0.0 && (0.0..=23.0).contains(&number)).then(|| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // an integral hour in 0..=23
                let hour = number as u32;
                hour
            })
        };
        let window = match (hour("startHour"), hour("endHour")) {
            (Some(start_hour), Some(end_hour)) if start_hour != end_hour => Some(Window {
                start_hour,
                end_hour,
            }),
            _ => None,
        };
        let cache = section("claudeCache");
        Self {
            enabled: flag("enabled"),
            always: flag("enabled") && flag("always"),
            window,
            hybrid_cache: cache.and_then(|cache| cache.get("enabled")) == Some(&Value::Bool(true))
                && cache
                    .and_then(|cache| cache.get("mode"))
                    .and_then(Value::as_str)
                    == Some("hybrid"),
        }
    }

    /// `isCacheKeepPersistentlyEnabled`.
    pub(crate) fn persistently_enabled(&self) -> bool {
        self.enabled && (self.always || self.window.is_some())
    }

    /// `isCacheKeepHybridActive`.
    pub(crate) fn hybrid_active(&self) -> bool {
        self.persistently_enabled() && self.hybrid_cache
    }

    /// `isCacheKeepActiveNow`.
    pub(crate) fn active_at(&self, now: &DateTime<FixedOffset>) -> bool {
        self.enabled
            && (self.always
                || self
                    .window
                    .is_some_and(|window| window.contains(now.hour())))
    }

    /// `cacheKeepPeriodKey`: `always`, else the local day the window opened.
    fn period_key(&self, now: &DateTime<FixedOffset>) -> String {
        if self.always {
            "always".to_string()
        } else {
            window_key(self.window, now)
        }
    }
}

/// `localDayKey`.
fn day_key(day: &DateTime<FixedOffset>) -> String {
    format!("{}-{:02}-{:02}", day.year(), day.month(), day.day())
}

/// `localWindowKey`: an overnight window's early hours belong to the day
/// it opened.
fn window_key(window: Option<Window>, now: &DateTime<FixedOffset>) -> String {
    match window {
        Some(window) if window.start_hour > window.end_hour && now.hour() < window.end_hour => {
            day_key(&(*now - Duration::days(1)))
        }
        _ => day_key(now),
    }
}

// ---------------------------------------------------------------------------
// The prewarm body
// ---------------------------------------------------------------------------

/// JavaScript truthiness of a JSON value.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `hasExplicitCacheControl`.
fn has_cache_breakpoint(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(has_cache_breakpoint),
        Value::Object(map) => {
            map.get("cache_control")
                .and_then(|control| control.get("type"))
                .and_then(Value::as_str)
                == Some("ephemeral")
                || map.values().any(has_cache_breakpoint)
        }
        _ => false,
    }
}

/// `buildCacheKeepPrewarmBody`: the request again with `max_tokens: 0`,
/// without `stream`, budgeted thinking, a structured output format or a
/// forced tool choice, in Claude Code's key order, the billing block's
/// `cch` reset to the native placeholder (`signRequestBody` at 7f5d88a).
pub(crate) fn prewarm_body(body_text: &str) -> Result<String, &'static str> {
    let Ok(body) = serde_json::from_str::<Value>(body_text) else {
        return Err("body is not valid JSON");
    };
    if !has_cache_breakpoint(&body) {
        return Err("body has no explicit cache breakpoints");
    }
    let Value::Object(mut warm) = body else {
        return Err("body has no explicit cache breakpoints");
    };
    warm.insert("max_tokens".to_string(), Value::from(0));
    warm.shift_remove("stream");
    if warm
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("enabled")
    {
        warm.shift_remove("thinking");
    }
    if warm
        .get("output_config")
        .and_then(|config| config.get("format"))
        .is_some_and(js_truthy)
    {
        warm.shift_remove("output_config");
    }
    if matches!(
        warm.get("tool_choice")
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str),
        Some("tool" | "any")
    ) {
        warm.shift_remove("tool_choice");
    }
    let ordered = anthropic::claude_code::order_claude_code_body(Value::Object(warm));
    Ok(anthropic::cch::reset_billing_header_cch(
        &anthropic::cch::js_json_stringify(&ordered),
    ))
}

/// `cacheKeepRetryDelayMs`: one minute doubling to fifteen, plus a
/// deterministic jitter per session and attempt.
pub(crate) fn retry_delay_ms(target_id: &str, failures: u32) -> i64 {
    let doublings = failures.saturating_sub(1).min(8);
    let exponential = RETRY_MAX_MS.min(RETRY_BASE_MS * (1_i64 << doublings));
    // FNV-1a over the code points' first UTF-16 units, as the plugin hashes
    // `${targetId}:${failureCount}` (`Math.imul`, then `>>> 0`).
    let mut hash: u32 = 2_166_136_261;
    for character in format!("{target_id}:{failures}").chars() {
        let mut units = [0_u16; 2];
        hash ^= u32::from(character.encode_utf16(&mut units)[0]);
        hash = hash.wrapping_mul(16_777_619);
    }
    let span = RETRY_JITTER_MAX_MS.min(exponential / 4) + 1;
    exponential + i64::from(hash) % span
}

// ---------------------------------------------------------------------------
// The tracked sessions
// ---------------------------------------------------------------------------

/// A session whose cache is kept (`CacheKeepTarget`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    /// The caller's session id.
    pub(crate) id: String,
    /// Where the request went.
    pub(crate) url: String,
    /// The request's headers (its credential left out: a prewarm sends
    /// the store's current one).
    pub(crate) headers: Vec<(String, String)>,
    /// The request's exact body.
    pub(crate) body_text: String,
    /// Its length in UTF-16 units (the plugin's memory budget unit).
    body_units: usize,
    /// The store row it was served with.
    pub(crate) account_id: String,
    pub(crate) cache_expires_at: i64,
    pub(crate) next_prewarm_at: i64,
    pub(crate) consecutive_failures: u32,
    day_key: String,
}

/// A tracked session as the registry and the status list it
/// (`CacheKeepTrackedSession`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedSession {
    pub(crate) id: String,
    pub(crate) cache_expires_at: i64,
    pub(crate) next_prewarm_at: i64,
}

/// A request to keep warm (`track`'s input).
#[derive(Debug, Clone)]
pub(crate) struct Track<'a> {
    pub(crate) session_id: Option<&'a str>,
    pub(crate) url: &'a str,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) body_text: &'a str,
    pub(crate) account_id: &'a str,
}

/// How one prewarm went (`CacheKeepPrewarmResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The cache was read and renewed.
    Warmed,
    /// The request cannot be prewarmed (no breakpoints, not JSON): the
    /// session is dropped.
    Skipped(&'static str),
    /// The provider answered an error (`status`) or the send failed
    /// (`None`): retried with backoff.
    Failed(Option<u16>),
}

/// What a tick found to do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tick {
    /// Nothing is tracked: the scheduler may park.
    Idle,
    /// The schedule or the cache mode is off now: nothing to send.
    Inactive,
    /// These sessions are due (in tracking order).
    Due(Vec<Target>),
}

/// The tracked sessions of this process (`CacheKeepManager`'s state).
#[derive(Debug, Default)]
pub(crate) struct CacheKeep {
    targets: Mutex<Vec<Target>>,
}

impl CacheKeep {
    /// `track`: keep `request`'s cache warm, when the settings ask for it
    /// now. `Err` says why not.
    pub(crate) fn track(
        &self,
        request: &Track<'_>,
        settings: &Settings,
        now: &DateTime<FixedOffset>,
    ) -> Result<(), &'static str> {
        let Some(session_id) = request.session_id.filter(|id| !id.is_empty()) else {
            return Err("missing session id");
        };
        if !settings.persistently_enabled() {
            return Err("cachekeep disabled");
        }
        if !settings.hybrid_active() {
            return Err("cache mode is not hybrid");
        }
        let body_units = request.body_text.encode_utf16().count();
        if body_units > MAX_BODY_UNITS {
            return Err("body exceeds cachekeep memory budget");
        }
        if !settings.active_at(now) {
            return Err("outside configured schedule");
        }
        let now_ms = now.timestamp_millis();
        let today = settings.period_key(now);
        let mut targets = self.targets.lock_or_recover();
        prune(&mut targets, now_ms, Some(&today));
        targets.retain(|target| target.id != session_id);
        targets.push(Target {
            id: session_id.to_string(),
            url: request.url.to_string(),
            headers: request
                .headers
                .iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
                .cloned()
                .collect(),
            body_text: request.body_text.to_string(),
            body_units,
            account_id: request.account_id.to_string(),
            cache_expires_at: now_ms + TTL_MS,
            next_prewarm_at: now_ms + TTL_MS - PREWARM_LEAD_MS,
            consecutive_failures: 0,
            day_key: today.clone(),
        });
        prune(&mut targets, now_ms, Some(&today));
        Ok(())
    }

    /// The first half of `runTick`: drop what expired or belongs to another
    /// day, then the sessions due now.
    pub(crate) fn begin_tick(&self, settings: &Settings, now: &DateTime<FixedOffset>) -> Tick {
        let now_ms = now.timestamp_millis();
        let today = settings.period_key(now);
        let mut targets = self.targets.lock_or_recover();
        if settings.always {
            for target in targets.iter_mut() {
                target.day_key = "always".to_string();
            }
        }
        prune(&mut targets, now_ms, Some(&today));
        if targets.is_empty() {
            return Tick::Idle;
        }
        if !settings.hybrid_active() || !settings.active_at(now) {
            return Tick::Inactive;
        }
        Tick::Due(
            targets
                .iter()
                .filter(|target| target.next_prewarm_at <= now_ms)
                .cloned()
                .collect(),
        )
    }

    /// Record how `id`'s prewarm, started at `now_ms`, went (`prewarm`).
    pub(crate) fn settle(&self, id: &str, outcome: &Outcome, now_ms: i64) {
        let mut targets = self.targets.lock_or_recover();
        match outcome {
            Outcome::Skipped(_) => targets.retain(|target| target.id != id),
            Outcome::Failed(_) => {
                if let Some(target) = targets.iter_mut().find(|target| target.id == id) {
                    target.consecutive_failures += 1;
                    target.next_prewarm_at =
                        now_ms + retry_delay_ms(&target.id, target.consecutive_failures);
                }
            }
            Outcome::Warmed => {
                if let Some(target) = targets.iter_mut().find(|target| target.id == id) {
                    target.cache_expires_at = now_ms + TTL_MS;
                    target.next_prewarm_at = target.cache_expires_at - PREWARM_LEAD_MS;
                    target.consecutive_failures = 0;
                }
            }
        }
    }

    /// `trackedSessions`: by id.
    pub(crate) fn tracked_sessions(&self) -> Vec<TrackedSession> {
        let mut sessions: Vec<TrackedSession> = self
            .targets
            .lock_or_recover()
            .iter()
            .map(|target| TrackedSession {
                id: target.id.clone(),
                cache_expires_at: target.cache_expires_at,
                next_prewarm_at: target.next_prewarm_at,
            })
            .collect();
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        sessions
    }

    /// The login `session`'s cache is kept warm on (`trackedOAuthRoute`).
    pub(crate) fn tracked_login(&self, session: &str) -> Option<String> {
        self.targets
            .lock_or_recover()
            .iter()
            .find(|target| target.id == session)
            .map(|target| target.account_id.clone())
    }

    /// How many sessions are tracked.
    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.targets.lock_or_recover().len()
    }
}

/// `pruneTargets`: other days' and expired sessions out, then the oldest
/// past the count and memory bounds.
fn prune(targets: &mut Vec<Target>, now_ms: i64, today: Option<&str>) {
    targets.retain(|target| {
        today.is_none_or(|today| target.day_key == today) && now_ms < target.cache_expires_at
    });
    while targets.len() > MAX_TARGETS {
        targets.remove(0);
    }
    while !targets.is_empty()
        && targets
            .iter()
            .map(|target| target.body_units)
            .sum::<usize>()
            > MAX_BODY_UNITS
    {
        targets.remove(0);
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The tracked sessions of every live process (`CacheKeepSessionRegistry`).
#[derive(Debug)]
pub(crate) struct Registry {
    directory: PathBuf,
    file: PathBuf,
}

/// The registry directory pi uses in this environment.
pub(crate) fn registry_dir_from_env() -> PathBuf {
    std::env::var_os(REGISTRY_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                std::env::temp_dir()
                    .join("opencode-anthropic-auth")
                    .join("cachekeep-sessions")
                    .join("pi")
            },
            PathBuf::from,
        )
}

impl Registry {
    /// This process's record in `directory`.
    pub(crate) fn new(directory: PathBuf) -> Self {
        let file = directory.join(format!(
            "{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        Self { directory, file }
    }

    /// `publish`: this process's sessions (none: its record removed).
    /// Blocking; best effort.
    pub(crate) fn publish(&self, sessions: &[TrackedSession], now_ms: i64) {
        if sessions.is_empty() {
            let _ = std::fs::remove_file(&self.file);
            return;
        }
        if let Err(error) = self.write(sessions, now_ms) {
            tracing::debug!(%error, "the cache keep-alive registry could not be written");
        }
    }

    fn write(&self, sessions: &[TrackedSession], now_ms: i64) -> std::io::Result<()> {
        create_private_dir(&self.directory)?;
        let text = format!("{}\n", record_text(sessions, now_ms));
        let temporary = PathBuf::from(format!(
            "{}.{}.tmp",
            self.file.display(),
            uuid::Uuid::new_v4()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&temporary).and_then(|mut file| {
            use std::io::Write;
            file.write_all(text.as_bytes())
        });
        let renamed = written.and_then(|()| std::fs::rename(&temporary, &self.file));
        if renamed.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        renamed
    }

    /// `list`: every live record's sessions (the newest expiry per id) and
    /// `local`'s, by id. Blocking.
    pub(crate) fn list(&self, local: &[TrackedSession], now_ms: i64) -> Vec<TrackedSession> {
        let mut sessions: std::collections::BTreeMap<String, TrackedSession> =
            std::collections::BTreeMap::new();
        let mut keep = |session: TrackedSession| match sessions.get(&session.id) {
            Some(existing) if existing.cache_expires_at >= session.cache_expires_at => {}
            _ => {
                sessions.insert(session.id.clone(), session);
            }
        };
        let names = std::fs::read_dir(&self.directory)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "json")
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for path in names {
            let Some(record) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            let Some(updated_at) = record_updated_at(&record) else {
                continue;
            };
            if now_ms - updated_at > REGISTRY_LEASE_MS || updated_at > now_ms {
                continue;
            }
            for session in record["sessions"].as_array().into_iter().flatten() {
                if let Some(session) = record_session(session) {
                    keep(session);
                }
            }
        }
        for session in local {
            keep(session.clone());
        }
        sessions.into_values().collect()
    }

    /// The directory the records live in.
    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}

/// One process's registry record (`CacheKeepRegistryRecord`), as
/// `JSON.stringify` writes it.
pub(crate) fn record_text(sessions: &[TrackedSession], now_ms: i64) -> String {
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|session| {
            serde_json::json!({
                "id": session.id,
                "cacheExpiresAt": session.cache_expires_at,
                "nextPrewarmAt": session.next_prewarm_at,
            })
        })
        .collect();
    serde_json::json!({ "version": 1, "updatedAt": now_ms, "sessions": sessions }).to_string()
}

/// A record's `updatedAt`, when it is a version-1 record with a session
/// list (`normalizeRecord`).
fn record_updated_at(record: &Value) -> Option<i64> {
    if record.get("version").and_then(Value::as_f64) != Some(1.0)
        || !record.get("sessions").is_some_and(Value::is_array)
    {
        return None;
    }
    let updated_at = record.get("updatedAt").and_then(Value::as_f64)?;
    #[allow(clippy::cast_possible_truncation)]
    // epoch milliseconds
    updated_at.is_finite().then_some(updated_at as i64)
}

/// A record's session, when well formed (`normalizeSession`).
fn record_session(session: &Value) -> Option<TrackedSession> {
    let id = session.get("id").and_then(Value::as_str)?;
    let time = |name: &str| {
        let value = session.get(name).and_then(Value::as_f64)?;
        #[allow(clippy::cast_possible_truncation)]
        // epoch milliseconds
        value.is_finite().then_some(value as i64)
    };
    (!id.is_empty())
        .then(|| TrackedSession {
            id: id.to_string(),
            cache_expires_at: time("cacheExpiresAt").unwrap_or(i64::MIN),
            next_prewarm_at: time("nextPrewarmAt").unwrap_or(i64::MIN),
        })
        .filter(|session| {
            session.cache_expires_at != i64::MIN && session.next_prewarm_at != i64::MIN
        })
}

/// Create `directory` (owner-only where it is new).
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory)
    }
}

// ---------------------------------------------------------------------------
// /claude-cachekeep
// ---------------------------------------------------------------------------

/// What `/claude-cachekeep`'s arguments ask for
/// (`parseCacheKeepCommandAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Status,
    Disable,
    Always,
    Window(Window),
    Subagents(bool),
    Usage,
}

/// `parseCacheKeepCommandAction`.
pub(crate) fn parse_command(args: &str) -> Action {
    let trimmed = args.trim();
    match trimmed {
        "" => return Action::Status,
        "off" => return Action::Disable,
        "always" => return Action::Always,
        "subagents on" => return Action::Subagents(true),
        "subagents off" => return Action::Subagents(false),
        _ => {}
    }
    // `^(\d{1,2})-(\d{1,2})$`.
    let hour = |text: &str| {
        (!text.is_empty() && text.len() <= 2 && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u32>().ok())
            .flatten()
    };
    match trimmed.split_once('-') {
        Some((start, end)) => match (hour(start), hour(end)) {
            (Some(start_hour), Some(end_hour))
                if start_hour <= 23 && end_hour <= 23 && start_hour != end_hour =>
            {
                Action::Window(Window {
                    start_hour,
                    end_hour,
                })
            }
            _ => Action::Usage,
        },
        None => Action::Usage,
    }
}

/// What the status shows (`CacheKeepStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) enabled: bool,
    pub(crate) always: bool,
    pub(crate) window: Option<Window>,
    pub(crate) hybrid_active: bool,
    pub(crate) sessions: Vec<TrackedSession>,
}

/// `toLocaleString()` in `en-US`: `5/18/2026, 10:55:00 AM`.
pub(crate) fn locale_time(at: &DateTime<FixedOffset>) -> String {
    let (pm, hour) = at.hour12();
    format!(
        "{}/{}/{}, {}:{:02}:{:02} {}",
        at.month(),
        at.day(),
        at.year(),
        hour,
        at.minute(),
        at.second(),
        if pm { "PM" } else { "AM" }
    )
}

/// `buildCacheKeepStatusSummary`.
fn status_summary(status: &Status, offset: FixedOffset) -> String {
    let schedule = if status.always {
        "always (while this process is running)".to_string()
    } else {
        status.window.map_or_else(
            || "not configured".to_string(),
            |window| format!("{:02}-{:02}", window.start_hour, window.end_hour),
        )
    };
    let mut lines = vec![
        format!(
            "Cache keep: {}",
            if status.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        format!("Schedule: {schedule}"),
        "Mode requirement: `/claude-cache mode hybrid` must be active.".to_string(),
        format!(
            "Hybrid active: {}",
            if status.hybrid_active { "yes" } else { "no" }
        ),
        format!("Tracked sessions: {}", status.sessions.len()),
    ];
    if !status.sessions.is_empty() {
        lines.push("Sessions:".to_string());
        lines.extend(
            status
                .sessions
                .iter()
                .map(|session| format!("- {}", session.id)),
        );
    }
    let next = status
        .sessions
        .iter()
        .map(|session| session.next_prewarm_at)
        .min()
        .filter(|next| *next != 0);
    if let Some(next) = next.and_then(DateTime::from_timestamp_millis) {
        lines.push(format!(
            "Next prewarm: {}",
            locale_time(&next.with_timezone(&offset))
        ));
    }
    lines.join("\n")
}

const USAGE: &str = "Usage: `/claude-cachekeep`, `/claude-cachekeep always`, `/claude-cachekeep off`, or `/claude-cachekeep HH-HH`.";

/// `executeCacheKeepCommand` (times in `offset`, the local zone).
pub(crate) fn command_text(action: Action, status: &Status, offset: FixedOffset) -> String {
    match action {
        Action::Status => format!(
            "## Claude Cache Keep Status\n\n{}",
            status_summary(status, offset)
        ),
        Action::Disable => format!(
            "## Claude Cache Keep Disabled\n\n{}",
            status_summary(
                &Status {
                    enabled: false,
                    ..status.clone()
                },
                offset
            )
        ),
        Action::Always => format!(
            "## Claude Cache Keep Enabled\n\n{}",
            status_summary(
                &Status {
                    enabled: true,
                    always: true,
                    window: None,
                    ..status.clone()
                },
                offset
            )
        ),
        Action::Window(window) => format!(
            "## Claude Cache Keep Enabled\n\n{}",
            status_summary(
                &Status {
                    enabled: true,
                    always: false,
                    window: Some(window),
                    ..status.clone()
                },
                offset
            )
        ),
        Action::Subagents(enabled) => format!(
            "## Claude Cache Keep Status\n\n{}\n\nSubagent tracking: {}",
            status_summary(status, offset),
            if enabled { "enabled" } else { "disabled" }
        ),
        Action::Usage => format!(
            "## Claude Cache Keep Usage\n\n{USAGE}\n\n{}",
            status_summary(status, offset)
        ),
    }
}

/// `/claude-cachekeep <args>`: the setting written (as the plugin's
/// `setCacheKeepPersistent*` write it), then the status over `sessions`
/// (every live process's tracked sessions, read after the write).
pub(crate) fn run_command(
    settings: &crate::pi::settings::PluginSettings,
    args: &str,
    sessions: impl FnOnce() -> Vec<TrackedSession>,
    offset: FixedOffset,
) -> Result<String, crate::pi::settings::SettingsError> {
    use crate::pi::commands::merge_section;
    let action = parse_command(args);
    match action {
        Action::Window(window) => settings.update(|config| {
            merge_section(
                config,
                "cacheKeep",
                [
                    ("enabled", Value::Bool(true)),
                    ("always", Value::Bool(false)),
                    ("startHour", Value::from(window.start_hour)),
                    ("endHour", Value::from(window.end_hour)),
                ],
            );
        })?,
        Action::Always => settings.update(|config| {
            merge_section(
                config,
                "cacheKeep",
                [
                    ("enabled", Value::Bool(true)),
                    ("always", Value::Bool(true)),
                ],
            );
            if let Some(Value::Object(keep)) = config.get_mut("cacheKeep") {
                keep.shift_remove("startHour");
                keep.shift_remove("endHour");
            }
        })?,
        Action::Disable => settings.update(|config| {
            merge_section(config, "cacheKeep", [("enabled", Value::Bool(false))]);
        })?,
        Action::Subagents(enabled) => settings.update(|config| {
            merge_section(config, "cacheKeep", [("subagents", Value::Bool(enabled))]);
        })?,
        Action::Status | Action::Usage => {}
    }
    let current = Settings::from_config(&settings.read());
    let status = Status {
        enabled: current.persistently_enabled(),
        always: current.always,
        window: current.window,
        hybrid_active: current.hybrid_active(),
        sessions: sessions(),
    };
    Ok(command_text(action, &status, offset))
}

#[cfg(test)]
mod tests;
