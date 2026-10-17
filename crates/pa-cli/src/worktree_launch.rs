//! `-w, --worktree [name]` (Claude Code's `--worktree`): the launch creates
//! or reuses a managed worktree (`pa_core::worktree`) and the session runs in
//! it; leaving the interactive session asks whether to remove it when it is
//! clean. The decisions live in `pa_core::worktree`; this module does the
//! launch's I/O (the directory change, the exit question).

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::Path;

use pa_core::worktree::{CreateRequest, ExitChoice, RemoveOutcome, Worktree, WorktreeHookContext};

/// Create (or reuse) the worktree for this launch and move the process into
/// it. A trusted launch directory carries its trust to the worktree when the
/// worktree's project configuration is identical.
///
/// # Errors
///
/// The worktree cannot be created or entered (the message is the user's).
pub(crate) fn enter(cwd: &Path, agent_dir: &Path, name: Option<&str>) -> Result<Worktree, String> {
    let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
    let worktree = pa_core::worktree::create(&CreateRequest {
        name: name.map(str::to_string),
        from: cwd.to_path_buf(),
        base: pa_core::worktree::configured_base(settings.settings()),
        hooks: Some(WorktreeHookContext {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            session_id: None,
        }),
    })
    .map_err(|error| format!("Error creating worktree: {error}"))?;
    std::env::set_current_dir(&worktree.path).map_err(|error| {
        format!(
            "Error: worktree directory {} cannot be entered: {error}",
            worktree.path.display()
        )
    })?;
    if let Err(error) = pa_core::workspace_trust::carry_trust(cwd, &worktree.path, agent_dir) {
        tracing::warn!(target: "pa_cli::worktree", %error, "worktree trust carry failed");
    }
    Ok(worktree)
}

/// After the interactive session: keep a worktree with work (or one the
/// agent still uses), and offer to remove a clean one this launch created.
/// Without a terminal to ask on, the worktree is kept.
pub(crate) fn finish(worktree: &Worktree, session_running: bool, agent_dir: &Path) {
    let location = describe(worktree);
    match pa_core::worktree::exit_choice(worktree, session_running) {
        ExitChoice::Keep { reason } => {
            eprintln!("Worktree kept at {location}: {reason}.");
        }
        ExitChoice::OfferRemoval => {
            if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
                eprintln!("Worktree kept at {location}.");
                return;
            }
            eprint!("Worktree {location} has no changes. Remove it? [Y/n] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            if std::io::stdin().lock().read_line(&mut answer).is_err() {
                eprintln!("\nWorktree kept at {location}.");
                return;
            }
            if !matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "" | "y" | "yes"
            ) {
                eprintln!("Worktree kept at {location}.");
                return;
            }
            let context = WorktreeHookContext {
                cwd: worktree.path.clone(),
                agent_dir: agent_dir.to_path_buf(),
                session_id: None,
            };
            match pa_core::worktree::remove(worktree, false, Some(&context)) {
                Ok(RemoveOutcome::Removed { branch_deleted }) => {
                    let branch = match (&worktree.branch, branch_deleted) {
                        (Some(branch), true) => format!(" and branch {branch}"),
                        (Some(branch), false) => {
                            format!(" (branch {branch} kept: its commits are on no other branch)")
                        }
                        (None, _) => String::new(),
                    };
                    eprintln!("Removed worktree {}{branch}.", worktree.path.display());
                }
                Ok(RemoveOutcome::Kept(changes)) => eprintln!(
                    "Worktree kept at {location}: it has {}.",
                    changes.describe(worktree.branch.as_deref())
                ),
                Err(error) => eprintln!("Worktree kept at {location}: {error}"),
            }
        }
    }
}

fn describe(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => format!("{} (branch {branch})", worktree.path.display()),
        None => worktree.path.display().to_string(),
    }
}
