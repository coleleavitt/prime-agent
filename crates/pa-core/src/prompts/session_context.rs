//! The session context the model receives as conversation rows instead of
//! system-prompt text (Claude Code's context attachments): the project
//! instruction files, the skills inventory, the generic MCP servers, the
//! environment, and today's date. The system prompt stays byte-stable for
//! the provider's prompt cache; this context rides the first turn as one
//! hidden row, and a later turn carries only what changed (an environment
//! update, a date change, instruction files changed or removed, skills added
//! or removed).
//!
//! Everything here is pure rendering. A row persists a [`ContextFingerprint`]
//! of what it announced (paths and hashes, never file contents), so the
//! next delta is computed against what the model was actually told, from the
//! conversation itself: a resumed session, a compaction that dropped the
//! first row, and a branch switch all work out what to send from the rows
//! they still hold.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::skills::Skill;

/// The fingerprint format a context row's details carry.
pub const CONTEXT_FINGERPRINT_VERSION: u32 = 1;

/// The suffix of every context row: the model should use it, not recite it
/// (Claude Code's `rD`).
pub const AMBIENT_CONTEXT_NOTE: &str = "This is ambient context \u{2014} do not narrate it to the user unless they ask or it is directly relevant to their request.";

/// Open and close of the wrapper every context and reminder row uses.
pub const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
pub const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// Wrap one row body in `<system-reminder>` tags (Claude Code's `Yl`).
#[must_use]
pub fn system_reminder(body: &str) -> String {
    format!("{SYSTEM_REMINDER_OPEN}\n{body}\n{SYSTEM_REMINDER_CLOSE}")
}

/// One instruction file (an `AGENTS.md`/`CLAUDE.md` the loader found).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// The path as announced to the model.
    pub path: String,
    pub content: String,
}

/// The environment lines, one field each. Embeddings that add a line (an
/// output style, a per-session scratch directory) add a field here and a
/// line in [`render_environment`] / [`render_environment_update`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EnvironmentFields {
    /// The session's working directory (forward slashes).
    pub working_directory: String,
    /// Additional working directories (`--add-dir`, `/add-dir`,
    /// `permissions.additionalDirectories`), forward slashes, in the order
    /// added; `/add-dir` mid-session is an environment update.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    /// `None` when not probed (a dump without a session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_git_repo: Option<bool>,
    /// The checked-out branch, when the working directory is in a repository
    /// on one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// `linux`, `macos`, `windows`, ...
    pub platform: String,
    /// The shell `bash()` runs commands with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// The model the session runs (`provider/id`); a switch is an update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The conversation log path; `None` is "not persisted".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_log: Option<String>,
    /// Whether the model accepts image input, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision_capable: Option<bool>,
    /// The OS sandbox's one line (`None`: sandbox off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// A per-session scratch directory for temporary files, when the
    /// embedding provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratch_directory: Option<String>,
}

/// Everything one context row states, with contents (the live side of a
/// comparison).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub instruction_files: Vec<InstructionFile>,
    /// The model-visible skills' inventory entries, in inventory order.
    pub skills: Vec<SkillEntry>,
    /// Enabled generic MCP servers (deduplicated, sorted).
    pub mcp_servers: Vec<String>,
    pub environment: EnvironmentFields,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// The typed agent definitions `rlm.spawn(agent=...)` can run
    /// ([`crate::agents::agent_listing`]); empty lists none.
    pub agent_types: String,
}

/// One skill's `<skill>` entry in the inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub xml: String,
}

/// What a context row announced: the comparison side a delta is computed
/// against, persisted in the row's details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextFingerprint {
    pub version: u32,
    /// `(path, sha256 of content)`, in announcement order.
    pub instruction_files: Vec<(String, String)>,
    /// `(name, sha256 of the entry)`.
    pub skills: Vec<(String, String)>,
    pub mcp_servers: Vec<String>,
    pub environment: EnvironmentFields,
    pub date: String,
    /// sha256 of the agent-type listing (empty: none was announced).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub agent_types: String,
}

fn sha256_hex(text: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(text.as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The inventory entries of the skills the model may invoke.
#[must_use]
pub fn skill_entries(skills: &[Skill]) -> Vec<SkillEntry> {
    skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .map(|skill| SkillEntry {
            name: skill.name.clone(),
            xml: crate::skills::skill_inventory_entry(skill),
        })
        .collect()
}

/// Deduplicated, sorted MCP server names.
#[must_use]
pub fn normalize_mcp_servers(servers: &[String]) -> Vec<String> {
    let mut servers = servers.to_vec();
    servers.sort();
    servers.dedup();
    servers
}

/// The project-instructions section for a set of instruction files: the
/// text the system prompt's tail used to carry. Reusable by any loader of
/// instruction files.
#[must_use]
pub fn render_instruction_files(files: &[InstructionFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Project Context".to_string(),
        String::new(),
        "Project-specific instructions and guidelines:".to_string(),
        String::new(),
    ];
    for file in files {
        lines.push(format!("## {}\n\n{}", file.path, file.content));
    }
    lines.join("\n")
}

/// Instruction files that changed or appeared since they were announced, and
/// the paths that are gone (Claude Code's `Rlt` delta form).
#[must_use]
pub fn render_instruction_files_update(changed: &[InstructionFile], removed: &[String]) -> String {
    let mut parts = Vec::new();
    if !changed.is_empty() {
        let mut body = vec![
            "These instruction files changed \u{2014} each replaces its earlier copy, and the project context above still applies otherwise:".to_string(),
        ];
        for file in changed {
            body.push(format!("## {}\n\n{}", file.path, file.content));
        }
        parts.push(body.join("\n\n"));
    }
    for path in removed {
        parts.push(format!("Instructions no longer present: {path}"));
    }
    parts.join("\n\n")
}

/// The skills inventory (the text the system prompt's tail used to carry).
#[must_use]
pub fn render_skills(entries: &[SkillEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines = crate::skills::skill_inventory_preamble();
    lines.push("<available_skills>".to_string());
    lines.extend(entries.iter().map(|entry| entry.xml.clone()));
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// Skills added (or changed) and removed since the inventory was announced.
#[must_use]
pub fn render_skills_update(added: &[SkillEntry], removed: &[String]) -> String {
    let mut parts = Vec::new();
    if !added.is_empty() {
        let mut lines = vec![
            "New or changed skills are now available (each replaces any earlier entry of the same name):".to_string(),
            "<available_skills>".to_string(),
        ];
        lines.extend(added.iter().map(|entry| entry.xml.clone()));
        lines.push("</available_skills>".to_string());
        parts.push(lines.join("\n"));
    }
    if !removed.is_empty() {
        parts.push(format!(
            "These skills are no longer available; do not use them: {}.",
            removed
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.join("\n\n")
}

/// The generic MCP guidance for the enabled servers.
#[must_use]
pub fn render_mcp_servers(servers: &[String]) -> String {
    if servers.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Generic MCP Connections".to_string(),
        String::new(),
        "Generic MCP connections are accessed through the pre-imported Python `mcp` object in the Python REPL, not as top-level native tool namespaces or installed Python skills.".to_string(),
        format!(
            "Enabled generic MCP servers: {}.",
            servers
                .iter()
                .map(|server| format!("`{server}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for server in servers {
        lines.push(format!(
            "For `{server}`, first discover its tools with `await mcp.list_tools(\"{server}\")`, then call one with `await mcp.call_tool(\"{server}\", \"<tool>\", arguments)`."
        ));
    }
    lines.join("\n")
}

fn vision_line(vision_capable: Option<bool>) -> Option<String> {
    match vision_capable {
        Some(true) => Some(
            "Image input: this model can see images; `attach_image` loads them into context."
                .to_string(),
        ),
        Some(false) => Some(
            "Image input: this model cannot see images; `attach_image` has the configured \
             vision-capable `imageModel` read them and returns its text description (it errors \
             when no such image model is configured)."
                .to_string(),
        ),
        None => None,
    }
}

fn conversation_log_line(log: Option<&str>) -> String {
    format!("Conversation log: {}", log.unwrap_or("not persisted"))
}

/// The scratchpad line (Claude Code's wording; the per-session scratchpad of
/// [`crate::session_engine::scratch::SessionScratch`]).
fn scratch_line(scratch: &str) -> String {
    format!(
        "Scratchpad directory: {scratch} \u{2014} always use it for temporary files (intermediate \
         results, scripts, outputs that don't belong in the project) instead of `/tmp` or other \
         system temp directories; it is session-specific, isolated from the project, and private \
         to you (`os.environ[\"{}\"]` in the kernel). Only use `/tmp` if the user explicitly \
         asks.",
        crate::session_engine::scratch::SCRATCHPAD_ENV
    )
}

fn additional_directory_lines(heading: &str, dirs: &[&String]) -> Vec<String> {
    let mut lines = vec![heading.to_string()];
    lines.extend(dirs.iter().map(|dir| format!(" - {dir}")));
    lines
}

/// The `# Environment` section.
#[must_use]
pub fn render_environment(fields: &EnvironmentFields) -> String {
    let mut lines = vec![
        "# Environment".to_string(),
        format!("Working directory: {}", fields.working_directory),
    ];
    if !fields.additional_directories.is_empty() {
        let dirs: Vec<&String> = fields.additional_directories.iter().collect();
        lines.extend(additional_directory_lines(
            "Additional working directories:",
            &dirs,
        ));
    }
    if let Some(is_git_repo) = fields.is_git_repo {
        lines.push(format!("Is a git repository: {is_git_repo}"));
    }
    if let Some(branch) = &fields.git_branch {
        lines.push(format!("Git branch: {branch}"));
    }
    lines.push(format!("Platform: {}", fields.platform));
    if let Some(shell) = &fields.shell {
        lines.push(format!("Shell: {shell}"));
    }
    if let Some(os_version) = &fields.os_version {
        lines.push(format!("OS version: {os_version}"));
    }
    if let Some(model) = &fields.model {
        lines.push(format!("Model: {model}"));
    }
    lines.push(conversation_log_line(fields.conversation_log.as_deref()));
    lines.extend(vision_line(fields.vision_capable));
    lines.extend(fields.sandbox.clone());
    lines.extend(fields.scratch_directory.as_deref().map(scratch_line));
    lines.join("\n")
}

/// The `# Environment update` section: one line per field that changed, empty
/// when nothing did (Claude Code's `qat`).
#[must_use]
pub fn render_environment_update(previous: &EnvironmentFields, next: &EnvironmentFields) -> String {
    let mut lines = Vec::new();
    if next.working_directory != previous.working_directory {
        lines.push(format!(
            "Working directory: {} (was {})",
            next.working_directory, previous.working_directory
        ));
    }
    let added: Vec<&String> = next
        .additional_directories
        .iter()
        .filter(|dir| !previous.additional_directories.contains(dir))
        .collect();
    if !added.is_empty() {
        lines.extend(additional_directory_lines(
            "Additional working directories added:",
            &added,
        ));
    }
    let removed: Vec<&String> = previous
        .additional_directories
        .iter()
        .filter(|dir| !next.additional_directories.contains(dir))
        .collect();
    if !removed.is_empty() {
        lines.extend(additional_directory_lines(
            "Additional working directories no longer available:",
            &removed,
        ));
    }
    if next.is_git_repo != previous.is_git_repo {
        if let Some(is_git_repo) = next.is_git_repo {
            lines.push(format!("Is a git repository: {is_git_repo}"));
        }
    }
    if next.git_branch != previous.git_branch {
        match (&next.git_branch, &previous.git_branch) {
            (Some(branch), Some(was)) => lines.push(format!("Git branch: {branch} (was {was})")),
            (Some(branch), None) => lines.push(format!("Git branch: {branch}")),
            (None, Some(was)) => lines.push(format!("Git branch: none (was {was})")),
            (None, None) => {}
        }
    }
    if next.platform != previous.platform {
        lines.push(format!("Platform: {}", next.platform));
    }
    if next.shell != previous.shell {
        lines.push(format!(
            "Shell: {}",
            next.shell.as_deref().unwrap_or("the default")
        ));
    }
    if next.os_version != previous.os_version {
        if let Some(os_version) = &next.os_version {
            lines.push(format!("OS version: {os_version}"));
        }
    }
    if next.model != previous.model {
        if let Some(model) = &next.model {
            lines.push(match &previous.model {
                Some(was) => format!("Model: {model} (was {was})"),
                None => format!("Model: {model}"),
            });
        }
    }
    if next.conversation_log != previous.conversation_log {
        lines.push(conversation_log_line(next.conversation_log.as_deref()));
    }
    if next.vision_capable != previous.vision_capable {
        lines.extend(vision_line(next.vision_capable));
    }
    if next.sandbox != previous.sandbox {
        lines.push(
            next.sandbox
                .clone()
                .unwrap_or_else(|| "OS sandbox: off.".to_string()),
        );
    }
    if next.scratch_directory != previous.scratch_directory {
        lines.push(match &next.scratch_directory {
            Some(scratch) => scratch_line(scratch),
            None => {
                "The scratch directory announced earlier is no longer available; use it no further."
                    .to_string()
            }
        });
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = vec!["# Environment update".to_string()];
    out.extend(lines);
    out.join("\n")
}

/// Today's date line (Claude Code's `Tlt`, first form).
#[must_use]
pub fn render_date(date: &str) -> String {
    format!("Today's date is {date}.")
}

/// The date-change line (Claude Code's `Tlt`, changed form).
#[must_use]
pub fn render_date_change(date: &str) -> String {
    format!(
        "The date has changed. Today's date is now {date}. No need to announce the new date \u{2014} the user's own clock shows it."
    )
}

/// What a session context is built from.
#[derive(Debug, Default)]
pub struct SessionContextInputs<'a> {
    pub instruction_files: Vec<InstructionFile>,
    pub skills: &'a [Skill],
    /// Active tools: the skills inventory needs file access (`python_repl`
    /// or `bash`), the MCP guidance needs `python_repl`. `None` is the
    /// shipped surface (`python_repl` only).
    pub selected_tools: Option<&'a [&'a str]>,
    pub mcp_servers: &'a [String],
    pub environment: EnvironmentFields,
    pub date: String,
    /// The agent-type listing (`None`: spawning is off or no type exists).
    pub agent_types: Option<String>,
}

impl SessionContext {
    /// Assemble the context the inputs describe.
    #[must_use]
    pub fn build(inputs: SessionContextInputs<'_>) -> Self {
        let default_tools = [pa_types::ai::PYTHON_TOOL_NAME];
        let tools = inputs.selected_tools.unwrap_or(&default_tools);
        let has_ipython = tools.contains(&pa_types::ai::PYTHON_TOOL_NAME);
        let has_file_access = has_ipython || tools.contains(&"bash");
        Self {
            instruction_files: inputs.instruction_files,
            skills: if has_file_access {
                skill_entries(inputs.skills)
            } else {
                Vec::new()
            },
            mcp_servers: if has_ipython {
                normalize_mcp_servers(inputs.mcp_servers)
            } else {
                Vec::new()
            },
            environment: inputs.environment,
            date: inputs.date,
            agent_types: inputs.agent_types.unwrap_or_default(),
        }
    }

    /// What this context states, for persisting with the row.
    #[must_use]
    pub fn fingerprint(&self) -> ContextFingerprint {
        ContextFingerprint {
            version: CONTEXT_FINGERPRINT_VERSION,
            instruction_files: self
                .instruction_files
                .iter()
                .map(|file| (file.path.clone(), sha256_hex(&file.content)))
                .collect(),
            skills: self
                .skills
                .iter()
                .map(|entry| (entry.name.clone(), sha256_hex(&entry.xml)))
                .collect(),
            mcp_servers: self.mcp_servers.clone(),
            environment: self.environment.clone(),
            date: self.date.clone(),
            agent_types: if self.agent_types.is_empty() {
                String::new()
            } else {
                sha256_hex(&self.agent_types)
            },
        }
    }

    /// The full first-turn row text, wrapped.
    #[must_use]
    pub fn render_full(&self) -> String {
        let sections = [
            render_instruction_files(&self.instruction_files),
            render_skills(&self.skills),
            render_mcp_servers(&self.mcp_servers),
            self.agent_types.clone(),
            render_environment(&self.environment),
            render_date(&self.date),
            AMBIENT_CONTEXT_NOTE.to_string(),
        ];
        system_reminder(
            &sections
                .iter()
                .filter(|section| !section.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    /// The delta row text against what `previous` announced, `None` when
    /// nothing changed.
    #[must_use]
    pub fn render_delta(&self, previous: &ContextFingerprint) -> Option<String> {
        let previous_files: std::collections::HashMap<&str, &str> = previous
            .instruction_files
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect();
        let changed_files: Vec<InstructionFile> = self
            .instruction_files
            .iter()
            .filter(|file| {
                previous_files.get(file.path.as_str()).copied() != Some(&*sha256_hex(&file.content))
            })
            .cloned()
            .collect();
        let removed_files: Vec<String> = previous
            .instruction_files
            .iter()
            .filter(|(path, _)| !self.instruction_files.iter().any(|file| &file.path == path))
            .map(|(path, _)| path.clone())
            .collect();

        let previous_skills: std::collections::HashMap<&str, &str> = previous
            .skills
            .iter()
            .map(|(name, hash)| (name.as_str(), hash.as_str()))
            .collect();
        let added_skills: Vec<SkillEntry> = self
            .skills
            .iter()
            .filter(|entry| {
                previous_skills.get(entry.name.as_str()).copied() != Some(&*sha256_hex(&entry.xml))
            })
            .cloned()
            .collect();
        let removed_skills: Vec<String> = previous
            .skills
            .iter()
            .filter(|(name, _)| !self.skills.iter().any(|entry| &entry.name == name))
            .map(|(name, _)| name.clone())
            .collect();

        let mcp = if self.mcp_servers == previous.mcp_servers {
            String::new()
        } else if self.mcp_servers.is_empty() {
            "The generic MCP servers listed earlier are no longer enabled; their guidance no longer applies.".to_string()
        } else {
            format!(
                "The enabled generic MCP servers changed; this replaces the earlier list.\n\n{}",
                render_mcp_servers(&self.mcp_servers)
            )
        };
        let current_agent_types = if self.agent_types.is_empty() {
            String::new()
        } else {
            sha256_hex(&self.agent_types)
        };
        let agent_types = if current_agent_types == previous.agent_types {
            String::new()
        } else if self.agent_types.is_empty() {
            "The agent types listed earlier are no longer available to `rlm.spawn(agent=...)`."
                .to_string()
        } else {
            format!(
                "The available agent types changed; this replaces the earlier list.\n\n{}",
                self.agent_types
            )
        };
        let date = if self.date == previous.date {
            String::new()
        } else {
            render_date_change(&self.date)
        };
        let sections = [
            render_instruction_files_update(&changed_files, &removed_files),
            render_skills_update(&added_skills, &removed_skills),
            mcp,
            agent_types,
            render_environment_update(&previous.environment, &self.environment),
            date,
        ];
        let body: Vec<&str> = sections
            .iter()
            .filter(|section| !section.is_empty())
            .map(String::as_str)
            .collect();
        if body.is_empty() {
            return None;
        }
        Some(system_reminder(&format!(
            "{}\n\n{AMBIENT_CONTEXT_NOTE}",
            body.join("\n\n")
        )))
    }
}

/// Today's UTC date as `YYYY-MM-DD`.
#[must_use]
pub fn today_utc() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    civil_date_from_days(days as i64)
}

/// `YYYY-MM-DD` for a day count since the Unix epoch (Howard Hinnant's
/// civil-from-days).
#[must_use]
pub fn civil_date_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
#[path = "session_context_tests.rs"]
mod tests;
