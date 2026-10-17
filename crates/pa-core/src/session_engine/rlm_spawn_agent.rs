//! The agent-definition and isolation half of an `rlm.spawn` admission:
//! `agent="name"` resolves a typed definition ([`crate::agents`]) and
//! fills the child's model and thinking level from it when the call left
//! them out; `cwd=` places the child; `isolation="worktree"` (from the call
//! or the definition) cuts a fresh git worktree of the parent's repository
//! and places the child in it. Every child host (the daemon's supervisor
//! children, the print host's in-process children) receives the resolved
//! request, so the behavior is one.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use super::rlm_host::RlmSpawnRequest;
use crate::agents::{AgentIsolation, AgentSearch, discover_agents};

/// What the session's spawns resolve against: its working directory and
/// agent directory.
#[derive(Debug, Clone)]
pub struct SpawnContext {
    pub search: AgentSearch,
}

/// The `rlm.spawn` kwargs this half owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SpawnAgentArgs {
    pub(crate) agent: Option<String>,
    pub(crate) isolation: Option<AgentIsolation>,
    pub(crate) cwd: Option<String>,
}

impl SpawnAgentArgs {
    /// Read the kwargs; a present value of the wrong shape is an error.
    pub(crate) fn from_kwargs(kwargs: &Map<String, Value>, operation: &str) -> Result<Self> {
        let string = |key: &str| -> Result<Option<String>> {
            match kwargs.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) if !value.trim().is_empty() => {
                    Ok(Some(value.trim().to_string()))
                }
                Some(_) => bail!("{operation} {key} must be a non-empty string"),
            }
        };
        let isolation = match string("isolation")? {
            None => None,
            Some(value) => Some(AgentIsolation::parse(&value).ok_or_else(|| {
                anyhow!("{operation} isolation must be \"worktree\", got {value:?}")
            })?),
        };
        Ok(Self {
            agent: string("agent")?,
            isolation,
            cwd: string("cwd")?,
        })
    }
}

/// Resolve `args` onto `request`: the definition (model and thinking fill
/// only what the call left out), the child's working directory, and the
/// isolation the child needs. Answers the isolation still to prepare.
///
/// # Errors
///
/// An agent name with no definition (the message lists the known types),
/// a definition effort that is not a thinking level, a `cwd` that is not
/// an existing directory, or a spawn context the session never set.
pub(crate) fn resolve_spawn_agent(
    request: &mut RlmSpawnRequest,
    args: SpawnAgentArgs,
    context: Option<&SpawnContext>,
) -> Result<Option<AgentIsolation>> {
    const OPERATION: &str = "rlm.spawn";
    let mut isolation = args.isolation;
    if let Some(name) = &args.agent {
        let context = context.ok_or_else(|| {
            anyhow!("{OPERATION} agent=: agent definitions are unavailable in this session")
        })?;
        let catalog = discover_agents(&context.search);
        let Some(definition) = catalog.get(name).cloned() else {
            let known: Vec<&str> = catalog.agents.iter().map(|a| a.name.as_str()).collect();
            bail!(
                "{OPERATION} agent {name:?} not found (known agent types: {}); definitions live in \
                 .prime/agents/ or .claude/agents/ of the project, or the user agents directories",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            );
        };
        if request.model.is_none() {
            request.model.clone_from(&definition.model);
        }
        if request.thinking.is_none() {
            if let Some(effort) = &definition.effort {
                let level =
                    crate::kernel::rlm_runtime::normalize_requested_rlm_subagent_thinking_level(
                        Some(effort),
                        OPERATION,
                    )
                    .map_err(|error| {
                        anyhow!(
                            "agent {name:?} ({}) has an invalid effort: {error}",
                            definition.path.display()
                        )
                    })?;
                request.thinking = level.map(str::to_string);
            }
        }
        isolation = isolation.or(definition.isolation);
        request.agent = Some(definition);
    }
    if let Some(cwd) = args.cwd {
        let base = context.map(|context| context.search.cwd.clone());
        let path = match base {
            Some(base) => base.join(&cwd),
            None => PathBuf::from(&cwd),
        };
        if !path.is_dir() {
            bail!(
                "{OPERATION} cwd {cwd:?} is not an existing directory ({})",
                path.display()
            );
        }
        request.cwd = Some(path.to_string_lossy().to_string());
    }
    Ok(isolation)
}

/// Cut the child's worktree from the repository its working directory
/// (the request's `cwd`, else the session's) belongs to, and place the
/// child in it. The worktree's name is `agent-<8 hex>`.
///
/// # Errors
///
/// The working directory is not in a git repository with a commit, or git
/// refuses the worktree.
pub(crate) fn prepare_worktree(
    request: &mut RlmSpawnRequest,
    context: Option<&SpawnContext>,
) -> Result<()> {
    let cwd = match (&request.cwd, context) {
        (Some(cwd), _) => PathBuf::from(cwd),
        (None, Some(context)) => context.search.cwd.clone(),
        (None, None) => bail!("rlm.spawn isolation=\"worktree\" needs the session's directory"),
    };
    let slug = format!("agent-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let worktree = crate::git_worktree::create_worktree(&cwd, &slug)
        .map_err(|error| anyhow!("rlm.spawn isolation=\"worktree\": {error:#}"))?;
    request.cwd = Some(
        crate::git_worktree::worktree_cwd(&worktree, Path::new(&cwd))
            .to_string_lossy()
            .to_string(),
    );
    request.worktree = Some(worktree);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RlmSpawnRequest {
        RlmSpawnRequest {
            prompt: "task".into(),
            name: Some("kid".into()),
            model: None,
            thinking: None,
            spawned_by_request_id: None,
            target: crate::kernel::rlm_runtime::RlmSpawnTarget::default(),
            plan_mode: false,
            token_budget: None,
            cwd: None,
            agent: None,
            worktree: None,
        }
    }

    fn context(root: &Path) -> SpawnContext {
        SpawnContext {
            search: AgentSearch {
                cwd: root.to_path_buf(),
                agent_dir: root.join("agent"),
                home: None,
            },
        }
    }

    #[test]
    fn a_definition_fills_only_what_the_call_left_out() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join(".prime/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("r.md"),
            "---\nname: reviewer\ndescription: d\nmodel: p/m\neffort: high\nisolation: worktree\n---\nbody",
        )
        .unwrap();
        let context = context(root.path());
        let args = || SpawnAgentArgs {
            agent: Some("reviewer".into()),
            isolation: None,
            cwd: None,
        };
        let mut filled = request();
        let isolation = resolve_spawn_agent(&mut filled, args(), Some(&context)).unwrap();
        assert_eq!(
            (
                filled.model.as_deref(),
                filled.thinking.as_deref(),
                isolation,
                filled.agent.as_ref().map(|agent| agent.name.as_str()),
            ),
            (
                Some("p/m"),
                Some("high"),
                Some(AgentIsolation::Worktree),
                Some("reviewer")
            )
        );
        let mut explicit = request();
        explicit.model = Some("x/y".into());
        explicit.thinking = Some("low".into());
        resolve_spawn_agent(&mut explicit, args(), Some(&context)).unwrap();
        assert_eq!(
            (explicit.model.as_deref(), explicit.thinking.as_deref()),
            (Some("x/y"), Some("low"))
        );

        let error = resolve_spawn_agent(
            &mut request(),
            SpawnAgentArgs {
                agent: Some("nobody".into()),
                ..SpawnAgentArgs::default()
            },
            Some(&context),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("rlm.spawn agent \"nobody\" not found (known agent types: reviewer)"),
            "{error:#}"
        );
    }

    #[test]
    fn kwargs_parse_and_cwd_resolves_against_the_session() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        let mut kwargs = Map::new();
        kwargs.insert("isolation".into(), Value::from("worktree"));
        kwargs.insert("cwd".into(), Value::from("sub"));
        let args = SpawnAgentArgs::from_kwargs(&kwargs, "rlm.spawn").unwrap();
        assert_eq!(
            args,
            SpawnAgentArgs {
                agent: None,
                isolation: Some(AgentIsolation::Worktree),
                cwd: Some("sub".into()),
            }
        );
        let mut placed = request();
        resolve_spawn_agent(&mut placed, args, Some(&context(root.path()))).unwrap();
        assert_eq!(
            placed.cwd,
            Some(root.path().join("sub").to_string_lossy().to_string())
        );
        kwargs.insert("isolation".into(), Value::from("vm"));
        assert!(SpawnAgentArgs::from_kwargs(&kwargs, "rlm.spawn").is_err());
    }

    #[test]
    fn a_worktree_child_is_placed_in_its_worktree() {
        let root = tempfile::tempdir().unwrap();
        crate::git_env::run_fixture_git(root.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(root.path().join("f"), "x").unwrap();
        crate::git_env::run_fixture_git(root.path(), &["add", "."]);
        crate::git_env::run_fixture_git(root.path(), &["commit", "-q", "-m", "i"]);
        let mut placed = request();
        prepare_worktree(&mut placed, Some(&context(root.path()))).unwrap();
        let worktree = placed.worktree.clone().unwrap();
        assert!(worktree.branch.starts_with("worktree-agent-"));
        assert_eq!(
            placed.cwd.as_deref().map(Path::new),
            Some(worktree.path.as_path())
        );
        assert!(crate::git_worktree::remove_worktree_if_unchanged(&worktree).unwrap());
    }
}
