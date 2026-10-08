//! Non-fatal auth notices for the user: a problem the request path worked
//! around, which the user should still hear about (a refreshed login that
//! could not be saved, a revoked login another one now replaces).
//!
//! The auth code raises a notice for a condition while it stands and clears
//! it when it ends; raising a standing condition again changes nothing, so
//! a per-request check raises it once. Every session of the process hears
//! each raised condition once: a session whose sink registers while it
//! stands hears it then, and re-registering (an engine rebuild) never
//! repeats it. Hosts route a session's notices to their surface (the
//! daemon's `auth_notice` session event, print mode's stderr). A notice
//! never carries a token value.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pa_types::sync::MutexExt;

/// One notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthNotice {
    /// The provider id it concerns.
    pub provider: String,
    /// The condition it reports, stable while the condition stands (the
    /// deduplication key within the provider).
    pub condition: String,
    /// What the user is told (no secret).
    pub message: String,
}

/// Where a session's notices go.
pub type AuthNoticeSink = Arc<dyn Fn(&AuthNotice) + Send + Sync>;

/// A registered sink, held weakly.
type WeakSink = Weak<dyn Fn(&AuthNotice) + Send + Sync>;

/// A raised condition and the id that tells it from an earlier raise of the
/// same condition.
struct Standing {
    id: u64,
    notice: AuthNotice,
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    standing: Vec<Standing>,
    /// Session -> its sink (held weakly: the session's owner keeps it).
    sinks: HashMap<String, WeakSink>,
    /// Session -> the raises it heard.
    heard: HashMap<String, HashSet<u64>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Deliver each `(sink, notice)` outside the registry lock (a sink may
/// emit frames that take other locks).
fn deliver(deliveries: Vec<(AuthNoticeSink, AuthNotice)>) {
    for (sink, notice) in deliveries {
        sink(&notice);
    }
}

/// Raise `notice`'s condition: every session hears it once. A condition
/// that already stands is not raised again.
pub fn raise_auth_notice(notice: AuthNotice) {
    let deliveries = {
        let mut registry = registry().lock_or_recover();
        if registry.standing.iter().any(|standing| {
            standing.notice.provider == notice.provider
                && standing.notice.condition == notice.condition
        }) {
            return;
        }
        registry.next_id += 1;
        let id = registry.next_id;
        registry.standing.push(Standing { id, notice });
        let Registry {
            sinks,
            heard,
            standing,
            ..
        } = &mut *registry;
        let notice = &standing[standing.len() - 1].notice;
        sinks.retain(|session, sink| {
            let alive = sink.strong_count() > 0;
            if !alive {
                heard.remove(session);
            }
            alive
        });
        sinks
            .iter()
            .filter_map(|(session, sink)| {
                heard.entry(session.clone()).or_default().insert(id);
                Some((sink.upgrade()?, notice.clone()))
            })
            .collect::<Vec<_>>()
    };
    deliver(deliveries);
}

/// The condition `condition` of `provider` ended (the login was saved, a
/// new login replaced the revoked one): a later raise is a new condition.
pub fn clear_auth_notice(provider: &str, condition: &str) {
    registry().lock_or_recover().standing.retain(|standing| {
        standing.notice.provider != provider || standing.notice.condition != condition
    });
}

/// Route session `session_id`'s notices to `sink` while the caller keeps
/// it alive (held weakly); replaces an earlier sink of the session. The
/// standing conditions the session has not heard are delivered now.
pub fn register_auth_notice_sink(session_id: &str, sink: &AuthNoticeSink) {
    let deliveries = {
        let mut registry = registry().lock_or_recover();
        registry
            .sinks
            .insert(session_id.to_string(), Arc::downgrade(sink));
        let Registry {
            standing, heard, ..
        } = &mut *registry;
        let heard = heard.entry(session_id.to_string()).or_default();
        standing
            .iter()
            .filter(|standing| heard.insert(standing.id))
            .map(|standing| (Arc::clone(sink), standing.notice.clone()))
            .collect::<Vec<_>>()
    };
    deliver(deliveries);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(provider: &str, condition: &str, message: &str) -> AuthNotice {
        AuthNotice {
            provider: provider.to_string(),
            condition: condition.to_string(),
            message: message.to_string(),
        }
    }

    /// A sink recording the messages it heard about `provider` (the
    /// registry is the process's: parallel tests raise their own).
    fn recording(provider: &'static str) -> (AuthNoticeSink, Arc<Mutex<Vec<String>>>) {
        let heard: Arc<Mutex<Vec<String>>> = Arc::default();
        let into = Arc::clone(&heard);
        let sink: AuthNoticeSink = Arc::new(move |notice: &AuthNotice| {
            if notice.provider == provider {
                into.lock_or_recover().push(notice.message.clone());
            }
        });
        (sink, heard)
    }

    #[test]
    fn each_session_hears_a_condition_once_until_it_ends() {
        let provider = "notice-test-once";
        let (first, first_heard) = recording(provider);
        register_auth_notice_sink("notice-session-a", &first);

        raise_auth_notice(notice(provider, "unsaved", "not saved"));
        raise_auth_notice(notice(provider, "unsaved", "not saved"));
        // A session that starts while it stands hears it, once, even when
        // its engine is rebuilt.
        let (second, second_heard) = recording(provider);
        register_auth_notice_sink("notice-session-b", &second);
        register_auth_notice_sink("notice-session-b", &second);
        // It ends, and comes back: a new condition.
        clear_auth_notice(provider, "unsaved");
        raise_auth_notice(notice(provider, "unsaved", "not saved again"));

        assert_eq!(
            *first_heard.lock_or_recover(),
            vec!["not saved".to_string(), "not saved again".to_string()]
        );
        assert_eq!(
            *second_heard.lock_or_recover(),
            vec!["not saved".to_string(), "not saved again".to_string()]
        );
        clear_auth_notice(provider, "unsaved");
    }

    #[test]
    fn a_dropped_sink_hears_nothing() {
        let provider = "notice-test-dropped";
        let (sink, heard) = recording(provider);
        register_auth_notice_sink("notice-session-dropped", &sink);
        drop(sink);

        raise_auth_notice(notice(provider, "revoked", "revoked"));

        assert!(heard.lock_or_recover().is_empty());
        clear_auth_notice(provider, "revoked");
    }
}
