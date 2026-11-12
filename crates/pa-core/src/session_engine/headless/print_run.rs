//! One print/json invocation's drive over its built root session: the
//! prompt loop (session commands run through the shared executor), the
//! goal and autonomous continuation loops, the turn-boundary compaction
//! arms, and the RLM quiescence drive that holds the run open until every
//! inline descendant settles (the #2270 fix). The embedding owns the
//! engine assembly, the output streams, and the exit code: it reads the
//! [`PrintRunOutcome`], renders the terminal result, then calls
//! [`PrintRun::finish`].

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::Subscription;
use pa_types::ai::Model;

use super::EventSink;
use super::autonomous::HeadlessAutonomous;
use super::boundary::TurnBoundary;
use super::goal::PrintGoalSurface;
use crate::autonomous::AgentAutonomousConfig;
use crate::session_engine::engine::SessionEngine;
use crate::session_engine::rlm_in_process::InlineSessionNode;
use crate::session_engine::rlm_in_process::inline::drive_to_quiescence;

/// What one print invocation configures beyond its built session.
pub struct PrintRunConfig {
    /// The json event stream; `None` runs in text mode (the loops run,
    /// nothing streams).
    pub events: Option<EventSink>,
    /// The autonomous run the CLI flags enable; `None` starts disabled
    /// (`/autonomous` can enable it live).
    pub autonomous: Option<AgentAutonomousConfig>,
    /// The session cwd (the autonomous gates run there).
    pub cwd: PathBuf,
    /// The agent dir (the global harness store).
    pub agent_dir: PathBuf,
}

/// How the prompts ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintRunOutcome {
    /// A session command failed: the raw error, the run exits 1 and no
    /// later prompt ran.
    CommandFailed(String),
    /// Every prompt ran and the session settled.
    Settled {
        /// The goal hit the no-progress continuation cap during this run
        /// (text mode reports it on stderr and exits 1).
        goal_no_progress_cap: bool,
        /// The autonomous exit contract's stderr line (exit 1 in both
        /// modes).
        autonomous_stderr: Option<String>,
    },
}

/// One print invocation's drive state.
pub struct PrintRun {
    engine: Arc<SessionEngine>,
    model: Model,
    api_key: Option<String>,
    root_node: Arc<InlineSessionNode>,
    global_harness_dir: PathBuf,
    goal: Arc<PrintGoalSurface>,
    autonomous: Arc<HeadlessAutonomous>,
    goal_accounting: Option<Subscription>,
    accounting: Option<Subscription>,
    boundary: TurnBoundary,
    goal_updated_at_start: Option<u64>,
}

impl PrintRun {
    /// Wire the goal and autonomous loops onto the built root session:
    /// the usage accounting, the composed natural-turn-end continuation
    /// hook, and the refine planning source.
    pub async fn start(
        engine: &Arc<SessionEngine>,
        model: &Model,
        api_key: Option<String>,
        root_node: &Arc<InlineSessionNode>,
        config: PrintRunConfig,
    ) -> Self {
        // The goal continuation surface: wired in every output mode — the loop runs in text mode too,
        // only silently.
        let goal = std::sync::Arc::new(match &config.events {
            Some(sink) => PrintGoalSurface::json(Arc::clone(sink)),
            None => PrintGoalSurface::text(),
        });
        goal.seed_publish_baseline(engine).await;
        let goal_updated_at_start = engine.goal_state().await.updated_at;
        let goal_accounting = goal.wire_accounting(engine, engine.session.agent()).await;
        // The autonomous run: the CLI flags enable it, a no-flag session starts disabled and
        // `/autonomous` rewrites it live.
        let autonomous = std::sync::Arc::new(match config.autonomous.as_ref() {
            Some(autonomous) => HeadlessAutonomous::from_config(autonomous, &config.cwd),
            None => HeadlessAutonomous::disabled(&config.cwd),
        });
        let accounting = autonomous.wire_accounting(engine.session.agent()).await;
        // The composed natural-turn-end hook: the goal arm first (exclusive priority), the
        // autonomous arm on the fall-through; both arms defer while inline-subagent work is
        // unsettled.
        super::continuation::wire_continuation_hook(
            engine,
            engine.session.agent(),
            model,
            &goal,
            &autonomous,
            Some(root_node),
        );
        let global_harness_dir = crate::refinement::get_global_harness_state_dir(&config.agent_dir);
        // `refine.preview` plans with the same model, key, and store the
        // boundary's refinement applies with (upstream #899).
        let refine_session_id = engine.session.session_id().await;
        engine
            .turn_boundary
            .set_refine_planning_source(std::sync::Arc::new({
                let model = model.clone();
                let api_key = api_key.clone();
                let global_harness_dir = global_harness_dir.clone();
                let session_id = refine_session_id;
                move || {
                    Some(
                        crate::session_engine::turn_boundary::RefinePlanningContext {
                            model: model.clone(),
                            global_harness_dir: global_harness_dir.clone(),
                            refine_call: crate::session_engine::refine::default_refiner_call(
                                api_key.clone(),
                                None,
                                session_id.clone(),
                            ),
                        },
                    )
                }
            }));
        let boundary = match &config.events {
            Some(sink) => TurnBoundary::json(Arc::clone(sink)),
            None => TurnBoundary::text(),
        };
        Self {
            engine: Arc::clone(engine),
            model: model.clone(),
            api_key,
            root_node: Arc::clone(root_node),
            global_harness_dir,
            goal,
            autonomous,
            goal_accounting: Some(goal_accounting),
            accounting: Some(accounting),
            boundary,
            goal_updated_at_start,
        }
    }

    /// Run the prompts in order, then hold the run open until the
    /// session and every inline descendant settle and every deferred
    /// continuation drained. The `@file` images ride their prompt.
    ///
    /// # Errors
    ///
    /// Returns the failure of a turn, a boundary compaction, or a
    /// continuation admission (the run aborts; [`PrintRun::finish`] is
    /// not owed).
    pub async fn run(
        &mut self,
        prompts: Vec<(String, Vec<pa_agent::types::ImageContent>)>,
    ) -> Result<PrintRunOutcome, String> {
        let Self {
            engine,
            model,
            api_key,
            root_node,
            global_harness_dir,
            goal,
            autonomous,
            boundary,
            ..
        } = self;
        let api_key = &*api_key;
        // The autonomous runtime state the session-command executor mutates —
        // `/autonomous` rewrites what the hook, accounting, and exit contract read.
        let autonomous_state = autonomous.state_handle();
        // A failed session command rejects the prompt wait: the raw error, exit 1.
        let mut command_failure: Option<String> = None;
        'prompts: for (prompt, images) in prompts {
            // Session commands never reach the model loop: the pre-turn boundary stays theirs to skip
            // and the prompt's turn never exists.
            if let Some(command) = engine.session.classify_session_command(&prompt) {
                let execution = super::session_command::execute_prompt_session_command(
                    engine,
                    goal,
                    model,
                    api_key.clone(),
                    global_harness_dir.clone(),
                    &autonomous_state,
                    &command,
                    &images,
                )
                .await;
                if let Some(error) = execution.error {
                    command_failure = Some(error);
                    break 'prompts;
                }
                // A `/goal` start scheduled its continuation as queued session input: the prompt wait
                // drains it inside the same wait.
                if let Some(continuation) = execution.continuation_message {
                    goal.run_session_command_continuation(
                        engine,
                        boundary,
                        model,
                        api_key.clone(),
                        global_harness_dir.clone(),
                        &continuation,
                    )
                    .await?;
                }
                // The same queue drain a settled turn gets: held continuations and armed steers run as
                // this prompt's follow-up turns.
                goal.drive_boundary(
                    engine,
                    boundary,
                    model,
                    api_key.clone(),
                    global_harness_dir.clone(),
                )
                .await?;
                continue;
            }
            boundary
                .run_pre_turn(engine, model, api_key.clone())
                .await?;
            engine
                .session
                .prompt_with_images(
                    &prompt,
                    images,
                    crate::session_engine::PromptOptions::default(),
                )
                .await
                .map_err(|error| format!("{error:#}"))?;
            engine.session.agent().wait_for_idle().await;
            boundary
                .run_at_settled_turn(engine, model, api_key.clone(), global_harness_dir.clone())
                .await?;
            goal.drive_boundary(
                engine,
                boundary,
                model,
                api_key.clone(),
                global_harness_dir.clone(),
            )
            .await?;
            // The autonomous arm runs only when the goal does not own the boundary (the goal arm takes
            // exclusive priority).
            if !engine.goal_owns_continuation_wakeup().await {
                // The held threshold continuation drains as this invocation's follow-up
                // turn; the stop surfaces only through the exit contract.
                autonomous
                    .drive_boundary(
                        engine,
                        boundary,
                        model,
                        api_key.clone(),
                        global_harness_dir.clone(),
                    )
                    .await
                    .map_err(|error| format!("{error:#}"))?;
            }
        }
        // The rejected prompt wait: no later prompts ran, and the disposal
        // drain still runs.
        if let Some(error) = command_failure {
            self.unsubscribe().await;
            return Ok(PrintRunOutcome::CommandFailed(error));
        }
        // The #2270 fix: print waits for RLM quiescence — every admitted
        // descendant settles and its replies and notices drain as turns —
        // before the terminal result is selected (json mode keeps streaming;
        // the subscription lives until after the drive). One drive loop then
        // keeps the run alive while deferred work exists: the deferred
        // goal/autonomous continuations deliver after the settle ("no
        // front": the settling child's terminal notice is read first), and
        // the boundary drain runs whatever turns it drained armed — a
        // threshold continuation, a budget steer, more descendants.
        loop {
            if drive_to_quiescence(
                root_node,
                engine,
                boundary,
                model,
                api_key.clone(),
                global_harness_dir.clone(),
            )
            .await?
            {
                continue;
            }
            if let Some(continuation) = engine.take_owed_goal_continuation().await {
                goal.publish_goal_update(engine).await;
                goal.run_queued_turn(
                    engine,
                    boundary,
                    model,
                    api_key.clone(),
                    global_harness_dir.clone(),
                    &continuation,
                )
                .await?;
                continue;
            }
            if autonomous
                .deliver_rlm_continuation(
                    engine,
                    boundary,
                    model,
                    api_key.clone(),
                    global_harness_dir.clone(),
                )
                .await?
            {
                continue;
            }
            // The boundary drain: turns it ran may spawn descendants or arm
            // more holds, so it reports whether it ran and the loop goes
            // around — the leading quiescence pass then waits for whatever
            // they left behind.
            let mut boundary_ran = goal
                .drive_boundary(
                    engine,
                    boundary,
                    model,
                    api_key.clone(),
                    global_harness_dir.clone(),
                )
                .await?;
            if !engine.goal_owns_continuation_wakeup().await {
                boundary_ran |= autonomous
                    .drive_boundary(
                        engine,
                        boundary,
                        model,
                        api_key.clone(),
                        global_harness_dir.clone(),
                    )
                    .await?;
            }
            if boundary_ran {
                continue;
            }
            break;
        }
        self.unsubscribe().await;
        let goal = self.engine.goal_state().await;
        Ok(PrintRunOutcome::Settled {
            goal_no_progress_cap: goal.status == pa_types::goal::GoalStatus::Error
                && goal.last_error.as_deref()
                    == Some(
                        crate::session_engine::goal_driver::CONTINUATION_NO_PROGRESS_CAP_REASON,
                    )
                && goal.updated_at != self.goal_updated_at_start,
            autonomous_stderr: self.autonomous.exit_stderr().await,
        })
    }

    async fn unsubscribe(&mut self) {
        if let Some(subscription) = self.goal_accounting.take() {
            subscription.unsubscribe().await;
        }
        if let Some(subscription) = self.accounting.take() {
            subscription.unsubscribe().await;
        }
    }

    /// The disposal drain, after the embedding rendered the outcome: a
    /// compact-trigger auto-refine no later boundary consumed runs here.
    pub async fn finish(mut self) {
        self.boundary
            .drain_compact_auto_refine_at_disposal(
                &self.engine,
                &self.model,
                self.api_key.clone(),
                self.global_harness_dir.clone(),
            )
            .await;
    }
}
