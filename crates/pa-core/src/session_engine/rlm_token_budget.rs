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
//! they settle (input, output, and cache tokens). The spend and grant
//! totals, and a child's own grant, persist in the session's artifact dir
//! ([`RLM_TOKEN_BUDGET_FILE`]): a resumed child keeps counting against the
//! grant it was spawned with, and a restarted root does not refill its
//! pool.

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

/// The file under a session's artifact dir that keeps its budget ledger,
/// so the totals survive worker restarts, resumes and daemon restarts.
pub const RLM_TOKEN_BUDGET_FILE: &str = "rlm-token-budget.json";

/// A session's durable budget totals (`rlm-token-budget.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmTokenBudgetLedger {
    /// The grant that funded this session (a subagent only): a resume
    /// that carries no allowance (a daemon restart) keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowance: Option<u64>,
    /// Tokens this session's own replies spent.
    #[serde(default)]
    pub spent: u64,
    /// Tokens granted to this session's children (never returned).
    #[serde(default)]
    pub granted: u64,
}

impl RlmTokenBudgetLedger {
    /// Read a ledger. A missing file is an empty ledger; an unreadable or
    /// malformed one is reported (logged) and read as empty.
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "pa_core::rlm_token_budget",
                    %error,
                    "the RLM token budget ledger is malformed; starting from zero"
                );
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(
                    target: "pa_core::rlm_token_budget",
                    %error,
                    "the RLM token budget ledger is unreadable; starting from zero"
                );
                Self::default()
            }
        }
    }

    fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        crate::settings::storage::atomic_write(path, &format!("{text}\n"))
    }
}

/// One session's budget state.
#[derive(Debug)]
pub struct RlmTokenBudget {
    config: RlmTokenBudgetConfig,
    allowance: RlmTokenAllowance,
    /// This session's depth (its children run at depth + 1).
    depth: u32,
    /// The spend and grant totals.
    ledger: std::sync::Mutex<RlmTokenBudgetLedger>,
    /// Where the ledger persists (`None`: in memory only).
    store: Option<std::path::PathBuf>,
}

impl RlmTokenBudget {
    /// An in-memory budget (no session artifact dir to persist into).
    #[must_use]
    pub fn new(config: RlmTokenBudgetConfig, allowance: RlmTokenAllowance, depth: u32) -> Self {
        RlmTokenBudget {
            config,
            allowance,
            depth,
            ledger: std::sync::Mutex::new(RlmTokenBudgetLedger {
                allowance: granted_allowance(allowance),
                ..RlmTokenBudgetLedger::default()
            }),
            store: None,
        }
    }

    /// A budget whose totals persist in `store`, resuming what an earlier
    /// lifetime of the session spent and granted.
    #[must_use]
    pub fn open(
        config: RlmTokenBudgetConfig,
        allowance: RlmTokenAllowance,
        depth: u32,
        store: std::path::PathBuf,
    ) -> Self {
        let ledger = RlmTokenBudgetLedger {
            allowance: granted_allowance(allowance),
            ..RlmTokenBudgetLedger::load(&store)
        };
        RlmTokenBudget {
            config,
            allowance,
            depth,
            ledger: std::sync::Mutex::new(ledger),
            store: Some(store),
        }
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, RlmTokenBudgetLedger> {
        use pa_types::sync::MutexExt;
        self.ledger.lock_or_recover()
    }

    /// The pool this session draws grants and (for a child) its own spend
    /// from.
    fn pool(&self) -> u64 {
        match self.allowance {
            RlmTokenAllowance::Root => self.config.total,
            RlmTokenAllowance::Granted(grant) => grant,
        }
    }

    fn remaining_in(&self, ledger: &RlmTokenBudgetLedger) -> u64 {
        let own = match self.allowance {
            RlmTokenAllowance::Root => 0,
            RlmTokenAllowance::Granted(_) => ledger.spent,
        };
        self.pool()
            .saturating_sub(ledger.granted)
            .saturating_sub(own)
    }

    /// What is left to grant: the pool minus earlier grants and, for a
    /// child, its own spend.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining_in(&self.ledger())
    }

    /// Record one settled reply's tokens. A ledger that cannot be written
    /// is logged: the reply already happened, and memory keeps counting.
    pub fn record_spend(&self, tokens: u64) {
        let mut ledger = self.ledger();
        ledger.spent = ledger.spent.saturating_add(tokens);
        if let Some(store) = &self.store {
            if let Err(error) = ledger.save(store) {
                tracing::warn!(
                    target: "pa_core::rlm_token_budget",
                    error = %format!("{error:#}"),
                    "the RLM token budget ledger could not be written"
                );
            }
        }
    }

    /// Draw the grant for one new child. Never returned to the pool.
    /// `requested` (`rlm.spawn(token_budget=)`) asks for an explicit grant,
    /// which must fit both what is left and the per-depth ceiling for the
    /// child's depth; without one the child gets what is left, capped by
    /// that ceiling.
    ///
    /// # Errors
    ///
    /// Errors when the pool cannot fund the child (nothing left, or less
    /// than `requested`), when `requested` exceeds the per-depth ceiling,
    /// and when the grant cannot be made durable (a restart would refill
    /// it). Every refusal refuses the spawn.
    pub fn reserve_child_grant(&self, requested: Option<u64>) -> anyhow::Result<u64> {
        let ceiling = self.config.per_depth.get(self.depth as usize).copied();
        let mut ledger = self.ledger();
        let left = self.remaining_in(&ledger);
        let spent_note = || match self.allowance {
            RlmTokenAllowance::Root => String::new(),
            RlmTokenAllowance::Granted(_) => {
                format!(" or spent ({} spent by this session)", ledger.spent)
            }
        };
        let grant = match requested {
            Some(requested) => {
                if let Some(ceiling) = ceiling.filter(|ceiling| requested > *ceiling) {
                    anyhow::bail!(
                        "rlm.spawn token_budget={requested} exceeds the {ceiling}-token cap on any single grant to a depth-{} subagent",
                        self.depth + 1
                    );
                }
                if requested > left {
                    anyhow::bail!(
                        "RLM token budget cannot fund token_budget={requested}: {left} tokens are left to grant ({} of {} already granted{})",
                        ledger.granted,
                        self.pool(),
                        spent_note()
                    );
                }
                requested
            }
            None => left.min(ceiling.unwrap_or(u64::MAX)),
        };
        if grant == 0 {
            anyhow::bail!(
                "RLM token budget exhausted: {} of {} tokens already granted{}; no budget is left for another subagent",
                ledger.granted,
                self.pool(),
                spent_note()
            );
        }
        let next = RlmTokenBudgetLedger {
            granted: ledger.granted.saturating_add(grant),
            ..ledger.clone()
        };
        if let Some(store) = &self.store {
            next.save(store).map_err(|error| {
                error.context("record the RLM token grant (the spawn is refused)")
            })?;
        }
        *ledger = next;
        Ok(grant)
    }

    /// A child has spent what it did not grant on: its run stops at this
    /// turn boundary. The root is never stopped.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        match self.allowance {
            RlmTokenAllowance::Root => false,
            RlmTokenAllowance::Granted(grant) => {
                let ledger = self.ledger();
                ledger.spent >= grant.saturating_sub(ledger.granted)
            }
        }
    }
}

fn granted_allowance(allowance: RlmTokenAllowance) -> Option<u64> {
    match allowance {
        RlmTokenAllowance::Root => None,
        RlmTokenAllowance::Granted(grant) => Some(grant),
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
            .map(|_| root.reserve_child_grant(None).unwrap())
            .collect();
        assert_eq!(grants, vec![400, 400, 200]);
        let refused = root.reserve_child_grant(None).unwrap_err();
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
            child.reserve_child_grant(None).unwrap(),
            100,
            "the depth-2 ceiling"
        );
        assert_eq!(child.remaining(), 50);
        child.record_spend(60);
        assert!(child.exhausted(), "310 spent of the 300 it kept");
        assert_eq!(child.remaining(), 0);
        assert!(child.reserve_child_grant(None).is_err());
    }

    /// The ledger is the durable record: a reopened budget resumes the
    /// spend and the grants, and the file holds exactly the totals.
    #[test]
    fn the_ledger_persists_spend_grants_and_the_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("artifacts").join(RLM_TOKEN_BUDGET_FILE);
        let child = RlmTokenBudget::open(
            config(1_000, &[400, 100]),
            RlmTokenAllowance::Granted(400),
            1,
            store.clone(),
        );
        child.record_spend(150);
        assert_eq!(child.reserve_child_grant(None).unwrap(), 100);
        drop(child);
        assert_eq!(
            RlmTokenBudgetLedger::load(&store),
            RlmTokenBudgetLedger {
                allowance: Some(400),
                spent: 150,
                granted: 100,
            }
        );
        let reopened = RlmTokenBudget::open(
            config(1_000, &[400, 100]),
            RlmTokenAllowance::Granted(400),
            1,
            store,
        );
        assert_eq!(reopened.remaining(), 150);
    }

    /// A grant that cannot be made durable refuses the spawn and leaves
    /// the pool as it was.
    #[test]
    fn an_unrecordable_grant_refuses_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        // The ledger's parent is a file: nothing can be written under it.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let root = RlmTokenBudget::open(
            config(1_000, &[]),
            RlmTokenAllowance::Root,
            0,
            blocker.join(RLM_TOKEN_BUDGET_FILE),
        );
        assert!(root.reserve_child_grant(None).is_err());
        assert_eq!(root.remaining(), 1_000);
    }
}
