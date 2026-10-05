//! The host-enforced RLM delegation token budget (upstream #1192).
//! `rlmMaxDepth` bounds how deep a recursion tree goes, not what it
//! spends: node count grows as `fanout^depth`, so a per-agent limit does
//! not bound the tree. The budget bounds what a chat may spend on
//! subagents; the session the user talks to is never capped by it.
//!
//! One rule holds the bound. A session holds a pool and every child it
//! spawns draws a grant from it that is never returned: the root's pool is
//! the configured total; a child's pool is its grant, minus what it spends
//! itself. A child stops at the first turn boundary after its own spend
//! reaches what it has not granted on, and refuses to spawn once its pool
//! is empty, so the sum over any subtree stays within the grant that
//! funded it. The optional per-depth schedule caps any single grant to a
//! child at that depth.
//!
//! Off unless the global `rlmTokenBudget` setting is set (TS v0.9.8 had no
//! budget). Spend is counted from the session's own assistant replies as
//! they settle (input, output, and cache tokens), in memory: a resumed
//! child restarts its own count, and the root's granted total resets with
//! its worker.

use std::sync::atomic::{AtomicU64, Ordering};

/// The configured budget (`rlmTokenBudget`): the root's delegation pool
/// and the optional per-depth grant ceilings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmTokenBudgetConfig {
    /// Tokens the whole subagent tree under one root may spend.
    pub total: u64,
    /// `per_depth[d]` caps any single grant to a child at depth `d + 1`;
    /// depths past the end are uncapped beyond the pool.
    pub per_depth: Vec<u64>,
}

impl RlmTokenBudgetConfig {
    /// Parse the setting: a token count, or `{ "total": n, "perDepth":
    /// [n, ...] }`. Anything else (and a zero total) is off.
    #[must_use]
    pub fn from_setting(value: &serde_json::Value) -> Option<Self> {
        let (total, per_depth) = match value {
            serde_json::Value::Number(total) => (total.as_u64()?, Vec::new()),
            serde_json::Value::Object(object) => (
                object.get("total")?.as_u64()?,
                match object.get("perDepth") {
                    None | Some(serde_json::Value::Null) => Vec::new(),
                    Some(serde_json::Value::Array(ceilings)) => ceilings
                        .iter()
                        .map(serde_json::Value::as_u64)
                        .collect::<Option<Vec<u64>>>()?,
                    Some(_) => return None,
                },
            ),
            _ => return None,
        };
        (total > 0).then_some(RlmTokenBudgetConfig { total, per_depth })
    }
}

/// Where a session's pool comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlmTokenAllowance {
    /// The root: its pool is the configured total, and its own spend is
    /// never capped.
    Root,
    /// A subagent funded by its parent's grant: its own spend and its
    /// grants to children both draw from it.
    Granted(u64),
}

/// One session's budget state.
#[derive(Debug)]
pub struct RlmTokenBudget {
    config: RlmTokenBudgetConfig,
    allowance: RlmTokenAllowance,
    /// This session's depth (its children run at depth + 1).
    depth: u32,
    /// Tokens this session's own replies spent.
    spent: AtomicU64,
    /// Tokens granted to this session's children.
    granted: AtomicU64,
}

impl RlmTokenBudget {
    #[must_use]
    pub fn new(config: RlmTokenBudgetConfig, allowance: RlmTokenAllowance, depth: u32) -> Self {
        RlmTokenBudget {
            config,
            allowance,
            depth,
            spent: AtomicU64::new(0),
            granted: AtomicU64::new(0),
        }
    }

    /// The pool this session draws grants and (for a child) its own spend
    /// from.
    fn pool(&self) -> u64 {
        match self.allowance {
            RlmTokenAllowance::Root => self.config.total,
            RlmTokenAllowance::Granted(grant) => grant,
        }
    }

    /// What is left to grant: the pool minus earlier grants and, for a
    /// child, its own spend.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        let own = match self.allowance {
            RlmTokenAllowance::Root => 0,
            RlmTokenAllowance::Granted(_) => self.spent.load(Ordering::SeqCst),
        };
        self.pool()
            .saturating_sub(self.granted.load(Ordering::SeqCst))
            .saturating_sub(own)
    }

    /// Record one settled reply's tokens.
    pub fn record_spend(&self, tokens: u64) {
        self.spent.fetch_add(tokens, Ordering::SeqCst);
    }

    /// Draw the grant for one new child: what is left, capped by the
    /// per-depth ceiling for the child's depth. Never returned to the pool.
    ///
    /// # Errors
    ///
    /// Errors when nothing is left to grant (the spawn is refused).
    pub fn reserve_child_grant(&self) -> anyhow::Result<u64> {
        let ceiling = self
            .config
            .per_depth
            .get(self.depth as usize)
            .copied()
            .unwrap_or(u64::MAX);
        loop {
            let granted = self.granted.load(Ordering::SeqCst);
            let left = self.remaining();
            let grant = left.min(ceiling);
            if grant == 0 {
                anyhow::bail!(
                    "RLM token budget exhausted: {} of {} tokens already granted{}; no budget is left for another subagent",
                    granted,
                    self.pool(),
                    match self.allowance {
                        RlmTokenAllowance::Root => String::new(),
                        RlmTokenAllowance::Granted(_) => format!(
                            " or spent ({} spent by this session)",
                            self.spent.load(Ordering::SeqCst)
                        ),
                    }
                );
            }
            if self
                .granted
                .compare_exchange(granted, granted + grant, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(grant);
            }
        }
    }

    /// A child has spent what it did not grant on: its run stops at this
    /// turn boundary. The root is never stopped.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        match self.allowance {
            RlmTokenAllowance::Root => false,
            RlmTokenAllowance::Granted(grant) => {
                self.spent.load(Ordering::SeqCst)
                    >= grant.saturating_sub(self.granted.load(Ordering::SeqCst))
            }
        }
    }
}

/// The tokens one settled reply spent: everything the provider processed.
#[must_use]
pub fn reply_tokens(usage: &pa_agent::types::Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(total: u64, per_depth: &[u64]) -> RlmTokenBudgetConfig {
        RlmTokenBudgetConfig {
            total,
            per_depth: per_depth.to_vec(),
        }
    }

    #[test]
    fn the_setting_parses_a_count_or_a_schedule_and_zero_is_off() {
        assert_eq!(
            RlmTokenBudgetConfig::from_setting(&serde_json::json!(400_000)),
            Some(config(400_000, &[]))
        );
        assert_eq!(
            RlmTokenBudgetConfig::from_setting(
                &serde_json::json!({ "total": 1_000_000, "perDepth": [400_000, 50_000] })
            ),
            Some(config(1_000_000, &[400_000, 50_000]))
        );
        for off in [
            serde_json::json!(0),
            serde_json::json!("400k"),
            serde_json::json!({ "perDepth": [1] }),
            serde_json::json!({ "total": 10, "perDepth": "x" }),
        ] {
            assert_eq!(RlmTokenBudgetConfig::from_setting(&off), None, "{off}");
        }
    }

    /// The root's grants drain its pool and are never returned; the
    /// per-depth ceiling caps each grant; an empty pool refuses.
    #[test]
    fn root_grants_draw_the_pool_down_under_the_depth_ceiling() {
        let root = RlmTokenBudget::new(config(1_000, &[400]), RlmTokenAllowance::Root, 0);
        root.record_spend(5_000);
        assert!(!root.exhausted(), "the root's own spend is never capped");
        let grants: Vec<u64> = (0..3)
            .map(|_| root.reserve_child_grant().unwrap())
            .collect();
        assert_eq!(grants, vec![400, 400, 200]);
        let refused = root.reserve_child_grant().unwrap_err();
        assert_eq!(
            refused.to_string(),
            "RLM token budget exhausted: 1000 of 1000 tokens already granted; no budget is left for another subagent"
        );
    }

    /// A child's own spend and its grants share its allowance: it stops
    /// once its spend reaches what it did not grant on, and its children
    /// get only what is left.
    #[test]
    fn a_child_spends_and_grants_from_one_allowance() {
        let child = RlmTokenBudget::new(
            config(1_000, &[400, 100]),
            RlmTokenAllowance::Granted(400),
            1,
        );
        child.record_spend(250);
        assert!(!child.exhausted());
        assert_eq!(
            child.reserve_child_grant().unwrap(),
            100,
            "the depth-2 ceiling"
        );
        assert_eq!(child.remaining(), 50);
        child.record_spend(60);
        assert!(child.exhausted(), "310 spent of the 300 it kept");
        assert_eq!(child.remaining(), 0);
        assert!(child.reserve_child_grant().is_err());
    }
}
