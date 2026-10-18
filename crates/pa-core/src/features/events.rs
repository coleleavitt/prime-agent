//! Session events features observe or answer beyond the tool path: a
//! project's setup run (`--init` / `--maintenance`), an MCP server's
//! elicitation, instruction files loading, a directory added to the
//! session, and the session's working directory moving. Each is Claude
//! Code's hook event of the same name; the types here are the seams'
//! vocabulary and the drivers compose the installed features' answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::{SessionFeature, SessionFeatureContext};

/// Why a setup run happens (Claude Code's `Setup` `trigger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupTrigger {
    /// `--init`: a project's first-time setup.
    Init,
    /// `--maintenance`: recurring upkeep.
    Maintenance,
}

impl SetupTrigger {
    /// Claude Code's spelling (`init` | `maintenance`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SetupTrigger::Init => "init",
            SetupTrigger::Maintenance => "maintenance",
        }
    }
}

/// A setup run ([`SessionFeature::on_setup`]): no session exists yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupObservation {
    pub trigger: SetupTrigger,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
}

/// Run every installed feature's [`SessionFeature::on_setup`] for a project
/// at `cwd`: the context they return, joined in installation order, for the
/// caller to carry into the session it starts (`None` when there is none).
/// The composition root calls this for `--init` / `--maintenance`.
pub async fn run_setup(trigger: SetupTrigger, cwd: &Path, agent_dir: &Path) -> Option<String> {
    run_setup_with(super::installed(), trigger, cwd, agent_dir).await
}

/// [`run_setup`] over `features`.
pub async fn run_setup_with(
    features: &[Arc<dyn SessionFeature>],
    trigger: SetupTrigger,
    cwd: &Path,
    agent_dir: &Path,
) -> Option<String> {
    let setup = SetupObservation {
        trigger,
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
    };
    let texts: Vec<String> =
        futures::future::join_all(features.iter().map(|feature| feature.on_setup(&setup)))
            .await
            .into_iter()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .collect();
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

/// How an MCP server asks the user for input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationMode {
    /// A form described by `requested_schema`.
    Form,
    /// A URL the user opens.
    Url,
}

impl ElicitationMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ElicitationMode::Form => "form",
            ElicitationMode::Url => "url",
        }
    }
}

/// An MCP server's request for user input (Claude Code's `Elicitation`).
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationObservation {
    pub server: String,
    pub message: String,
    pub mode: ElicitationMode,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    /// The form's JSON schema (form mode).
    pub requested_schema: Option<Value>,
}

/// The answer an elicitation gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl ElicitationAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ElicitationAction::Accept => "accept",
            ElicitationAction::Decline => "decline",
            ElicitationAction::Cancel => "cancel",
        }
    }

    /// Parse Claude Code's spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accept" => Some(ElicitationAction::Accept),
            "decline" => Some(ElicitationAction::Decline),
            "cancel" => Some(ElicitationAction::Cancel),
            _ => None,
        }
    }
}

/// An elicitation's answer: the action and, for an accepted form, its
/// content.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<Value>,
}

/// The first installed feature's answer for an elicitation, in
/// installation order: the MCP client sends it instead of asking the user.
/// `None` asks the user.
pub async fn answer_elicitation(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    request: &ElicitationObservation,
) -> Option<ElicitationResponse> {
    futures::future::join_all(
        features
            .iter()
            .map(|feature| feature.review_elicitation(context, request)),
    )
    .await
    .into_iter()
    .flatten()
    .next()
}

/// The response an elicitation sends after the user (or a feature)
/// answered: each installed feature may override it, in installation order
/// (Claude Code's `ElicitationResult`).
pub async fn settle_elicitation_result(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    request: &ElicitationObservation,
    mut response: ElicitationResponse,
) -> ElicitationResponse {
    for feature in features {
        if let Some(overridden) = feature
            .review_elicitation_result(context, request, &response)
            .await
        {
            response = overridden;
        }
    }
    response
}

/// Why an instruction file loaded (Claude Code's `load_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionLoadReason {
    /// Loaded when the session started.
    SessionStart,
    /// A nested directory's file, found when a tool touched a path under it.
    NestedTraversal,
    /// A conditional rule whose `paths:` glob matched a touched path.
    PathGlobMatch,
    /// Imported by another file's `@path`.
    Include,
}

impl InstructionLoadReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InstructionLoadReason::SessionStart => "session_start",
            InstructionLoadReason::NestedTraversal => "nested_traversal",
            InstructionLoadReason::PathGlobMatch => "path_glob_match",
            InstructionLoadReason::Include => "include",
        }
    }
}

/// One instruction file that loaded ([`SessionFeature::on_instructions_loaded`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionsLoadedObservation {
    pub path: PathBuf,
    /// Claude Code's `memory_type`: `User`, `Project`, `Local` or `Managed`.
    pub memory_type: &'static str,
    pub load_reason: InstructionLoadReason,
    /// A conditional rule's `paths:` globs.
    pub globs: Vec<String>,
    /// The path whose touch loaded it (nested and glob loads).
    pub trigger_file_path: Option<PathBuf>,
    /// The file whose `@path` imported it.
    pub parent_file_path: Option<PathBuf>,
}

impl InstructionsLoadedObservation {
    /// A session-start load of `file` (an import is an `include`).
    #[must_use]
    pub fn at_session_start(file: &crate::resources::instructions::InstructionFile) -> Self {
        use crate::resources::instructions::InstructionKind;
        Self {
            path: file.path.clone(),
            memory_type: match file.kind {
                InstructionKind::User => "User",
                InstructionKind::Project => "Project",
                InstructionKind::Local => "Local",
            },
            load_reason: if file.parent.is_some() {
                InstructionLoadReason::Include
            } else {
                InstructionLoadReason::SessionStart
            },
            globs: file.globs.clone(),
            trigger_file_path: None,
            parent_file_path: file.parent.clone(),
        }
    }
}

/// Tell `features` instruction files loaded; nothing happens for none.
pub(crate) fn observe_instructions_loaded(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    files: &[InstructionsLoadedObservation],
) {
    if files.is_empty() {
        return;
    }
    for feature in features {
        feature.on_instructions_loaded(context, files);
    }
}

/// How a directory joined the session (Claude Code's `DirectoryAdded`
/// `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryAddedSource {
    /// `/add-dir`.
    SlashCommand,
}

impl DirectoryAddedSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DirectoryAddedSource::SlashCommand => "slash_command",
        }
    }
}

/// Tell `features` a directory joined the session.
pub(crate) fn observe_directory_added(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    directory: &Path,
    source: DirectoryAddedSource,
) {
    for feature in features {
        feature.on_directory_added(context, directory, source);
    }
}

/// Tell `features` the session's working directory moved to `cwd`.
pub(crate) fn observe_cwd_changed(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    cwd: &Path,
) {
    for feature in features {
        feature.on_cwd_changed(context, cwd);
    }
}

#[cfg(test)]
mod tests {
    use super::super::FeatureFuture;
    use super::*;

    struct Answering(&'static str);

    impl SessionFeature for Answering {
        fn name(&self) -> &'static str {
            self.0
        }

        fn on_setup(&self, setup: &SetupObservation) -> FeatureFuture<Option<String>> {
            let text = format!("{} ran {}", self.0, setup.trigger.as_str());
            Box::pin(async move { Some(text) })
        }

        fn review_elicitation(
            &self,
            _context: &Arc<SessionFeatureContext>,
            _request: &ElicitationObservation,
        ) -> FeatureFuture<Option<ElicitationResponse>> {
            let answer = (self.0 == "second").then_some(ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(serde_json::json!({ "name": "x" })),
            });
            Box::pin(async move { answer })
        }

        fn review_elicitation_result(
            &self,
            _context: &Arc<SessionFeatureContext>,
            _request: &ElicitationObservation,
            response: &ElicitationResponse,
        ) -> FeatureFuture<Option<ElicitationResponse>> {
            let overridden = (self.0 == "first").then(|| ElicitationResponse {
                action: ElicitationAction::Decline,
                content: response.content.clone(),
            });
            Box::pin(async move { overridden })
        }
    }

    #[tokio::test]
    async fn setup_and_elicitation_answers_compose_in_installation_order() {
        let features: Vec<Arc<dyn SessionFeature>> =
            vec![Arc::new(Answering("first")), Arc::new(Answering("second"))];
        assert_eq!(
            run_setup_with(
                &features,
                SetupTrigger::Init,
                Path::new("/w"),
                Path::new("/a")
            )
            .await,
            Some("first ran init\n\nsecond ran init".to_string())
        );
        assert_eq!(
            run_setup_with(
                &[],
                SetupTrigger::Maintenance,
                Path::new("/w"),
                Path::new("/a")
            )
            .await,
            None
        );
        let context = Arc::new(super::super::tests::context("elicitation"));
        let request = ElicitationObservation {
            server: "github".to_string(),
            message: "Your name?".to_string(),
            mode: ElicitationMode::Form,
            url: None,
            elicitation_id: None,
            requested_schema: Some(serde_json::json!({ "type": "object" })),
        };
        let answer = answer_elicitation(&features, &context, &request)
            .await
            .unwrap();
        assert_eq!(answer.action, ElicitationAction::Accept);
        assert_eq!(
            settle_elicitation_result(&features, &context, &request, answer).await,
            ElicitationResponse {
                action: ElicitationAction::Decline,
                content: Some(serde_json::json!({ "name": "x" })),
            }
        );
    }
}
