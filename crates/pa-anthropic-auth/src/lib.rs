//! Anthropic subscription auth through the shared account store.
//!
//! The fork's opencode and pi plugins keep Anthropic logins in one
//! machine-wide store (`~/.anthropic-accounts/accounts.json`, through
//! anthropic-napi); this crate makes prime-agent the third consumer of the
//! same store, through the same Rust SDK (`vendor/anthropic`). One login
//! serves every tool, and one refresh protocol rotates it: two custodians
//! of the same Anthropic login would each spend the single-use refresh
//! token and revoke the other's.
//!
//! [`install`] puts the store in charge of the `anthropic` provider id
//! through pa-core's generic credential source seam and pa-ai's provider
//! request hooks; with no login in the store the native `auth.json` path is
//! untouched, and a request whose token the store did not serve is sent as
//! it is. [`AnthropicAuthFeature`]
//! reports adoption once per process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pa_agent::types::AgentMessage;
use pa_core::features::{
    FeatureCommandOutcome, FeatureFuture, FeatureStatus, SessionFeature, SessionFeatureContext,
};
use pa_telemetry::Properties;
use pa_types::slash_commands::{BuiltinSlashCommand, SlashCommandExecution};
use pa_types::sync::MutexExt;

mod cachekeep;
mod config;
mod custody;
mod device;
mod hooks;
mod keepalive;
mod login;
mod pi;
mod quota;
mod revoked;
mod routing;
mod shape;
mod source;
#[cfg(test)]
mod test_support;

pub use login::{NewLogin, StoredLogin};
pub use pi::PiConfig;
pub use quota::QUOTA_RESERVE_ENV;
pub use source::{SharedStoreConfig, SharedStoreSource, SourceUsage, STORE_LABEL};

/// The provider id the store serves.
pub const PROVIDER_ID: &str = "anthropic";

/// The adoption event (`pa-telemetry`'s catalogue, v4).
pub const TELEMETRY_EVENT: &str = "anthropic_shared_auth";

/// The process's store source, configured from the environment on first
/// use (no I/O).
#[must_use]
pub fn shared_source() -> Arc<SharedStoreSource> {
    static SOURCE: OnceLock<Arc<SharedStoreSource>> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let source = Arc::new(SharedStoreSource::new(SharedStoreConfig::from_env()));
            source.attach();
            source
        })
        .clone()
}

/// Install the process's store source for [`PROVIDER_ID`]: its credential
/// source (pa-core) and its request hooks (pa-ai). Called by the
/// composition root before any session or worker starts; idempotent; no
/// I/O.
pub fn install() {
    pa_core::auth::install_credential_source(PROVIDER_ID, shared_source());
    pa_ai::request_hooks::install_request_hooks(PROVIDER_ID, shared_source());
}

/// The session feature that reports the store's adoption: once per
/// process, at the end of the first agent run after the store answered a
/// request. Never an account id, email, label or token.
pub struct AnthropicAuthFeature {
    source: Arc<SharedStoreSource>,
    reported: AtomicBool,
    /// The quota line last published per session.
    published: Mutex<HashMap<String, String>>,
}

impl AnthropicAuthFeature {
    /// The feature over `source` (the installed [`shared_source`]).
    #[must_use]
    pub fn new(source: Arc<SharedStoreSource>) -> Self {
        Self {
            source,
            reported: AtomicBool::new(false),
            published: Mutex::new(HashMap::new()),
        }
    }

    /// Publish the store's quota for an Anthropic session when it changed:
    /// the agents view shows the line (prime-agent has no other usage
    /// surface). Never an account id.
    fn publish_quota(&self, context: &SessionFeatureContext) {
        if context.model.provider != PROVIDER_ID {
            return;
        }
        let Some(quota) = self.source.quota_line() else {
            return;
        };
        let mut published = self.published.lock_or_recover();
        if published.get(&context.session_id) == Some(&quota.line) {
            return;
        }
        let delivered = pa_core::features::publish_feature_status(
            &context.session_id,
            FeatureStatus {
                feature: self.name().to_string(),
                line: Some(quota.line.clone()),
                status: quota.status,
            },
        );
        if delivered {
            published.insert(context.session_id.clone(), quota.line);
        }
    }
}

/// The feature's slash commands: name, description, argument hint (`None`:
/// takes no argument).
const COMMANDS: [(&str, &str, Option<&str>); 6] = [
    (
        pi::commands::FAST_COMMAND,
        pi::commands::FAST_DESCRIPTION,
        Some(pi::commands::FAST_HINT),
    ),
    (
        pi::commands::CACHE_COMMAND,
        pi::commands::CACHE_DESCRIPTION,
        Some(pi::commands::CACHE_HINT),
    ),
    (
        cachekeep::COMMAND,
        cachekeep::DESCRIPTION,
        Some(cachekeep::HINT),
    ),
    (
        pi::account_commands::ROUTING_COMMAND,
        pi::account_commands::ROUTING_DESCRIPTION,
        Some(pi::account_commands::ROUTING_HINT),
    ),
    (
        pi::account_commands::KILLSWITCH_COMMAND,
        pi::account_commands::KILLSWITCH_DESCRIPTION,
        Some(pi::account_commands::KILLSWITCH_HINT),
    ),
    (
        pi::account_commands::QUOTA_COMMAND,
        pi::account_commands::QUOTA_DESCRIPTION,
        None,
    ),
];

impl SessionFeature for AnthropicAuthFeature {
    fn name(&self) -> &'static str {
        "anthropic-auth"
    }

    /// The plugins' commands: `/claude-fast`, `/claude-cache` and
    /// `/claude-cachekeep` (request settings), `/claude-routing`, `/claude-killswitch` and
    /// `/claude-quota` (the store's logins).
    fn slash_commands(&self) -> Vec<BuiltinSlashCommand> {
        COMMANDS
            .iter()
            .map(|&(name, description, hint)| BuiltinSlashCommand {
                name,
                description,
                execution: SlashCommandExecution::Session,
                argument_hint: hint,
                aliases: &[],
                takes_argument: hint.is_some(),
            })
            .collect()
    }

    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        let name = COMMANDS
            .iter()
            .map(|(known, _, _)| *known)
            .find(|known| *known == name)?;
        // The sticky routing key: the top-level session this process serves.
        let session = self
            .source
            .session()
            .unwrap_or_else(|| context.session_id.clone());
        let (source, args) = (Arc::clone(&self.source), args.to_string());
        Some(Box::pin(async move {
            // The settings file is written under the plugins' lock; the
            // store and the routing state are read and written on disk.
            tokio::task::spawn_blocking(move || source.run_command(name, &args, &session))
                .await
                .map_err(|error| error.to_string())?
                .map(|text| FeatureCommandOutcome {
                    text,
                    completion: None,
                })
        }))
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, _history: &[AgentMessage]) {
        // The session this process serves is the sticky routing key; a
        // child agent's session rides its parent's login.
        if context.rlm_depth == 0 {
            *self.source.session.lock_or_recover() = Some(context.session_id.clone());
        }
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        self.publish_quota(context);
        let Some(telemetry) = &context.telemetry else {
            return;
        };
        let usage = self.source.usage();
        if !usage.answered() || self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut properties = Properties::new();
        properties.set("source", usage.first.unwrap_or("failed").into());
        properties.set("refreshed", usage.refreshed.into());
        properties.set("failed", usage.failed.into());
        properties.set("migrated", usage.migrated.into());
        properties.set("recovered", usage.recovered.into());
        properties.set("rotated", usage.rotated.into());
        let (polled, poll_failed) = self.source.quota.poll_counts();
        properties.set("polled", polled.into());
        properties.set("poll_failed", poll_failed.into());
        let counts = &self.source.counts;
        for (name, count) in [
            ("quota_routed", &counts.quota_routed),
            ("blocked", &counts.blocked),
            ("sticky_assigned", &counts.sticky_assigned),
            ("sticky_migrated", &counts.sticky_migrated),
        ] {
            properties.set(name, count.load(Ordering::SeqCst).into());
        }
        telemetry.track(TELEMETRY_EVENT, &properties);
    }
}
