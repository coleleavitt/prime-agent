//! One prewarm, and the scheduler that runs them (pi's `CacheKeepManager`
//! with its `prepareHeaders`): the tracked request's body made
//! prewarm-safe, the store's current token for the login the session was
//! served with, pi's headers over it, sent to where the request went.

use std::sync::Weak;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Instant;

use anthropic::access::{AccessRequest, get_access_token};
use anthropic::claude_code::{FAST_MODE_BETA, merge_anthropic_betas};
use anthropic::{AccountStore, SharedRefreshOptions};
use chrono::{DateTime, FixedOffset};
use pa_types::sync::MutexExt;
use serde_json::Value;

use super::{EXTENDED_TTL_BETA, Outcome, Settings, Target, Tick, Track, prewarm_body};
use crate::SharedStoreSource;
use crate::shape::{ShapeEnv, ShapeIdentity, claude_code_headers};
use crate::source::block_on_own_runtime;

/// Tracked headers a prewarm never copies (its own credential and framing
/// replace them).
const NOT_COPIED: [&str; 3] = ["authorization", "content-length", "transfer-encoding"];

/// A prewarm's headers (pi's `prepareHeaders`): Claude Code's set for the
/// prewarm body over the tracked request's headers (its own betas dropped,
/// so the tuple is chosen anew by the prewarm body), the extended cache
/// TTL beta merged, and fast mode's for a `speed: "fast"` body.
pub(crate) fn prewarm_headers(
    tracked: &[(String, String)],
    token: &str,
    body: &Value,
    identity: &ShapeIdentity,
    version: &str,
    env: &ShapeEnv,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut headers = claude_code_headers(token, body, identity, version, env, "", request_id);
    if let Some((_, betas)) = headers
        .iter_mut()
        .find(|(name, _)| name == "anthropic-beta")
    {
        let mut extra = vec![EXTENDED_TTL_BETA];
        if body.get("speed").and_then(Value::as_str) == Some("fast") {
            extra.push(FAST_MODE_BETA);
        }
        *betas = merge_anthropic_betas(betas, &extra);
    }
    for (name, value) in tracked {
        let set = NOT_COPIED
            .iter()
            .any(|skipped| name.eq_ignore_ascii_case(skipped))
            || name.eq_ignore_ascii_case("anthropic-beta")
            || headers
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(name));
        if !set {
            headers.push((name.clone(), value.clone()));
        }
    }
    headers
}

/// Whether `url` points at this machine (a loopback-only configuration
/// sends nowhere else).
fn is_loopback(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}

impl SharedStoreSource {
    /// Keep `request`'s cache warm when the settings ask for it (pi's
    /// `track` before each send). Prompt: in memory; the registry is
    /// written on the keep-alive's own thread.
    pub(crate) fn track_cachekeep(
        &self,
        request: &pa_ai::request_hooks::OutgoingRequest<'_>,
        account_id: &str,
    ) {
        let settings = Settings::from_config(&self.pi.settings.read());
        if !settings.persistently_enabled() {
            return;
        }
        let Some(body_text) = request.body.as_deref() else {
            return;
        };
        let url = format!(
            "{}/v1/messages",
            request.model.base_url.trim_end_matches('/')
        );
        let tracked = self.cachekeep.track(
            &Track {
                session_id: request.source.session_id,
                url: &url,
                headers: request.headers,
                body_text,
                account_id,
            },
            &settings,
            &chrono::Local::now().fixed_offset(),
        );
        match tracked {
            Ok(()) => self.cachekeep_changed(),
            Err(reason) => {
                tracing::debug!(reason, "a request was not tracked for cache keep-alive");
            }
        }
    }

    /// The tracked sessions changed: the thread republishes them (here,
    /// when no thread runs).
    fn cachekeep_changed(&self) {
        self.start_cachekeep();
        match self.cachekeep_jobs.get() {
            Some(sender) if sender.send(()).is_ok() => {}
            _ => self.publish_cachekeep(chrono::Utc::now().timestamp_millis()),
        }
    }

    /// Write this process's registry record.
    pub(crate) fn publish_cachekeep(&self, now_ms: i64) {
        self.cachekeep_registry
            .publish(&self.cachekeep.tracked_sessions(), now_ms);
    }

    /// Every live process's tracked sessions (this one's included).
    pub(crate) fn cachekeep_sessions(&self) -> Vec<super::TrackedSession> {
        self.cachekeep_registry.list(
            &self.cachekeep.tracked_sessions(),
            chrono::Utc::now().timestamp_millis(),
        )
    }

    /// One scheduler tick at `now` (`runTick`): the due sessions prewarmed
    /// in turn, then the registry rewritten. `true` when nothing is tracked
    /// any more (the scheduler may park).
    pub(crate) fn cachekeep_tick(&self, now: &DateTime<FixedOffset>) -> bool {
        let settings = Settings::from_config(&self.pi.settings.read());
        let now_ms = now.timestamp_millis();
        let tick = self.cachekeep.begin_tick(&settings, now);
        let idle = tick == Tick::Idle;
        if let Tick::Due(targets) = tick {
            for target in targets {
                let outcome = self.prewarm(&target);
                match &outcome {
                    Outcome::Warmed => {}
                    Outcome::Failed(status) => {
                        tracing::warn!(status = ?status, "a cache prewarm failed; retrying later");
                    }
                    Outcome::Skipped(reason) => {
                        tracing::debug!(reason, "a session's cache cannot be prewarmed; dropped");
                    }
                }
                self.cachekeep.settle(&target.id, &outcome, now_ms);
            }
        }
        self.publish_cachekeep(now_ms);
        idle
    }

    /// The store's current token for `account_id` (refreshed under the
    /// store's claim when expired), and the identity its requests carry.
    fn prewarm_login(&self, account_id: &str) -> Option<(String, ShapeIdentity)> {
        let grant = {
            let _flight = self.flight.lock_or_recover();
            block_on_own_runtime(get_access_token(
                self.client(),
                &self.config.store_path,
                &AccessRequest {
                    account: Some(account_id.to_string()),
                    ..AccessRequest::default()
                },
                &SharedRefreshOptions::default(),
            ))
            .ok()?
            .ok()?
        };
        if grant.account_id != account_id {
            return None;
        }
        let account_uuid = AccountStore::load(&self.config.store_path)
            .ok()
            .and_then(|store| {
                store
                    .get(account_id)?
                    .oauth()?
                    .account
                    .as_ref()
                    .map(|account| account.uuid.clone())
            })
            .filter(|uuid| !uuid.trim().is_empty());
        let identity = ShapeIdentity {
            device_id: self.load_device_id(),
            account_uuid,
            session_id: self.session_id(account_id),
        };
        Some((grant.access_token, identity))
    }

    /// Prewarm one tracked session (`sendPrewarm`). Blocking: refreshes
    /// the login's token when due, then sends (bounded).
    pub(crate) fn prewarm(&self, target: &Target) -> Outcome {
        let body_text = match prewarm_body(&target.body_text) {
            Ok(body_text) => body_text,
            Err(reason) => return Outcome::Skipped(reason),
        };
        if self.config.require_loopback && !is_loopback(&target.url) {
            return Outcome::Failed(None);
        }
        let Some((token, identity)) = self.prewarm_login(&target.account_id) else {
            tracing::warn!("the store has no token for a kept cache's login");
            return Outcome::Failed(None);
        };
        let body: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let headers = prewarm_headers(
            &target.headers,
            &token,
            &body,
            &identity,
            &self.claude_code_version(),
            &ShapeEnv::from_env(),
            &uuid::Uuid::new_v4().to_string(),
        );
        let url = target.url.clone();
        let sent = block_on_own_runtime(async move {
            let client = reqwest::Client::builder()
                .timeout(super::PREWARM_TIMEOUT)
                .build()
                .map_err(|error| error.to_string())?;
            let mut request = client.post(url).body(body_text);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Ok::<_, String>((status, text))
        });
        match sent {
            Ok(Ok((status, text))) if (200..300).contains(&status) => {
                let usage = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|data| data.get("usage").cloned());
                let count = |name: &str| {
                    usage
                        .as_ref()
                        .and_then(|usage| usage.get(name))
                        .and_then(Value::as_u64)
                };
                tracing::debug!(
                    input_tokens = count("input_tokens"),
                    cache_read_input_tokens = count("cache_read_input_tokens"),
                    cache_creation_input_tokens = count("cache_creation_input_tokens"),
                    "a cache prewarm succeeded"
                );
                Outcome::Warmed
            }
            Ok(Ok((status, _))) => Outcome::Failed(Some(status)),
            Ok(Err(_)) | Err(_) => Outcome::Failed(None),
        }
    }
}

/// The scheduler on the crate's own thread: a tick a minute while sessions
/// are tracked (`setInterval(tick, 60_000)`), parked while none are;
/// a tracked request wakes it and republishes the registry. It ends with
/// the source.
pub(crate) fn run(source: &Weak<SharedStoreSource>, wake_ups: &Receiver<()>) {
    let mut parked = false;
    let mut next_tick = Instant::now() + super::TICK;
    loop {
        let woken = if parked {
            match wake_ups.recv() {
                Ok(()) => true,
                Err(_) => return,
            }
        } else {
            match wake_ups.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        let Some(source) = source.upgrade() else {
            return;
        };
        if woken {
            if parked {
                parked = false;
                next_tick = Instant::now() + super::TICK;
            }
            source.publish_cachekeep(chrono::Utc::now().timestamp_millis());
            continue;
        }
        parked = source.cachekeep_tick(&chrono::Local::now().fixed_offset());
        next_tick = Instant::now() + super::TICK;
    }
}
