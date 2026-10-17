//! `/permissions`: the effective rules with their sources (Claude Code's
//! `/permissions` lists them in tabs), and the commands that change them.
//!
//! - `/permissions` (or `list`): mode, fallback, rules by behavior, working
//!   directories, sources, problems.
//! - `/permissions mode [<mode>]`: show or switch the mode.
//! - `/permissions allow|ask|deny <rule>`: add a rule (local settings file
//!   in a trusted workspace, this session otherwise).
//! - `/permissions remove <rule>`: remove a rule from the local settings
//!   file and this session.
//! - `/permissions check <rule-shaped action>`: what `Bash(git push)`,
//!   `Edit(src/a.rs)`, `WebFetch(https://...)` would get now.

use std::fmt::Write as _;

use crate::actions::from_probe;
use crate::decide::Decision;
use crate::mode::PermissionMode;
use crate::rule::{RuleBehavior, RuleValue, Validation, validate};
use crate::settings::{LOCAL_SETTINGS_FILE, SourceKind};
use crate::state::{Saved, SessionState};

/// The command's name.
pub const COMMAND: &str = "permissions";

/// What the rules can and cannot gate, said once in the listing.
const SCOPE_NOTE: &str = "Rules govern the kernel's bash() commands, file edits through the edit \
                          skill (file.edit), web.* requests and MCP tool calls. Python code the \
                          kernel runs directly (open(), subprocess, os) is not gated.";

/// One parsed `/permissions` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionsCommand {
    List,
    Mode(Option<String>),
    Add(RuleBehavior, String),
    Remove(String),
    Check(String),
}

/// Parse the command's arguments.
///
/// # Errors
///
/// An unknown subcommand or a missing argument.
pub fn parse(args: &str) -> Result<PermissionsCommand, String> {
    let args = args.trim();
    let (head, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(head, rest)| (head, rest.trim()));
    let needs = |what: &str| -> Result<String, String> {
        if rest.is_empty() {
            Err(format!("/permissions {head} needs {what}"))
        } else {
            Ok(rest.to_string())
        }
    };
    match head {
        "" | "list" => Ok(PermissionsCommand::List),
        "mode" => Ok(PermissionsCommand::Mode(
            (!rest.is_empty()).then(|| rest.to_string()),
        )),
        "remove" | "rm" => needs("a rule").map(PermissionsCommand::Remove),
        "check" => needs("an action such as Bash(git push)").map(PermissionsCommand::Check),
        other => match RuleBehavior::parse(other) {
            Some(behavior) => needs("a rule").map(|rule| PermissionsCommand::Add(behavior, rule)),
            None => Err(format!(
                "unknown /permissions subcommand {other:?}: use list, mode, allow, ask, deny, \
                 remove or check"
            )),
        },
    }
}

/// The listing.
#[must_use]
pub fn listing(state: &SessionState) -> String {
    let settings = &state.settings;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Permission mode: {} ({}); actions no rule covers: {}",
        state.mode(),
        state.mode().title(),
        settings.fallback.as_str()
    );
    let rules: Vec<_> = settings.rules.iter().chain(&state.session_rules).collect();
    for behavior in [RuleBehavior::Deny, RuleBehavior::Ask, RuleBehavior::Allow] {
        let of: Vec<_> = rules
            .iter()
            .filter(|rule| rule.behavior == behavior)
            .collect();
        let title = match behavior {
            RuleBehavior::Deny => "Deny",
            RuleBehavior::Ask => "Ask",
            RuleBehavior::Allow => "Allow",
        };
        if of.is_empty() {
            let _ = writeln!(out, "{title}: none");
            continue;
        }
        let _ = writeln!(out, "{title}:");
        for rule in of {
            let _ = writeln!(out, "  {}  ({})", rule.value, rule.source.label());
        }
    }
    let mut dirs = vec![state.paths.cwd.display().to_string()];
    dirs.extend(
        settings
            .additional_directories
            .iter()
            .map(|dir| dir.display().to_string()),
    );
    let _ = writeln!(out, "Working directories: {}", dirs.join(", "));
    let _ = writeln!(out, "Sources:");
    for (kind, path, exists) in &settings.files {
        let _ = writeln!(
            out,
            "  {}: {}{}",
            kind.label(),
            path.display(),
            if *exists { "" } else { " (none)" }
        );
    }
    if !settings.claude_code {
        let _ = writeln!(
            out,
            "Claude Code settings (~/.claude, .claude/) are not read; \"permissions\": \
             {{\"claudeCodeSettings\": true}} in user settings reads them."
        );
    }
    if !settings.project_trusted {
        let _ = writeln!(
            out,
            "The workspace is not trusted: project files contribute deny and ask rules only."
        );
    }
    let problems: Vec<&String> = settings
        .problems
        .iter()
        .chain(&state.policy().problems)
        .collect();
    if !problems.is_empty() {
        let _ = writeln!(out, "Problems:");
        for problem in problems {
            let _ = writeln!(out, "  {problem}");
        }
    }
    out.push_str(SCOPE_NOTE);
    out
}

/// Run one invocation against `state`; `Ok` is the result text, and
/// whether the published status changed.
///
/// # Errors
///
/// The invocation is malformed or refused.
pub fn run(state: &mut SessionState, args: &str) -> Result<(String, bool), String> {
    state.refresh();
    match parse(args)? {
        PermissionsCommand::List => {
            state.reload();
            Ok((listing(state), false))
        }
        PermissionsCommand::Mode(None) => Ok((
            format!(
                "Permission mode: {} ({}). Modes: {}.",
                state.mode(),
                state.mode().title(),
                PermissionMode::ALL
                    .iter()
                    .map(|mode| mode.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false,
        )),
        PermissionsCommand::Mode(Some(name)) => {
            let mode = PermissionMode::parse(&name).ok_or_else(|| {
                format!(
                    "unknown permission mode {name:?}: use {}",
                    PermissionMode::ALL
                        .iter()
                        .map(|mode| mode.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            state.set_mode(mode)?;
            let note = if mode == PermissionMode::Plan {
                " File edits are refused; /plan on also confines the kernel (shift+tab switches \
                 both)."
            } else {
                ""
            };
            Ok((
                format!("Permission mode set to {mode} ({}).{note}", mode.title()),
                true,
            ))
        }
        PermissionsCommand::Add(behavior, rule) => {
            if let Validation::Invalid { error, suggestion } = validate(&rule, behavior) {
                return Err(format!(
                    "invalid rule {rule:?}: {error}{}",
                    suggestion.map_or_else(String::new, |hint| format!(" ({hint})"))
                ));
            }
            let canonical = RuleValue::parse(&rule).to_string();
            let saved = state.add_rule(behavior, &canonical);
            let place = match saved {
                Saved::LocalFile => format!("to {LOCAL_SETTINGS_FILE}"),
                Saved::Session(reason) => format!("for this session ({reason})"),
            };
            Ok((
                format!("Added {} rule {canonical} {place}.", behavior.key()),
                true,
            ))
        }
        PermissionsCommand::Remove(rule) => {
            let canonical = RuleValue::parse(&rule).to_string();
            if state.remove_rule(&canonical)? {
                Ok((format!("Removed {canonical}."), true))
            } else {
                let elsewhere = state
                    .settings
                    .rules
                    .iter()
                    .find(|known| known.value.to_string() == canonical)
                    .map(|known| known.source);
                Err(match elsewhere {
                    Some(source) if source != SourceKind::Local => format!(
                        "{canonical} comes from {}; edit that file to remove it",
                        source.label()
                    ),
                    _ => format!("no rule {canonical} in {LOCAL_SETTINGS_FILE} or this session"),
                })
            }
        }
        PermissionsCommand::Check(probe) => {
            let action = from_probe(&RuleValue::parse(&probe), &state.paths.anchors());
            let verdict = match state.decide(&action) {
                Decision::Allow => "allowed".to_string(),
                Decision::Ask { reason, remember } => format!(
                    "asks: {reason}{}",
                    remember.map_or_else(String::new, |rule| format!(
                        " (always allowing adds {rule})"
                    ))
                ),
                Decision::Deny { reason } => format!("denied: {reason}"),
            };
            Ok((format!("{probe}: {verdict}"), false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_parse_into_subcommands() {
        assert_eq!(parse(""), Ok(PermissionsCommand::List));
        assert_eq!(parse(" list "), Ok(PermissionsCommand::List));
        assert_eq!(parse("mode"), Ok(PermissionsCommand::Mode(None)));
        assert_eq!(
            parse("mode acceptEdits"),
            Ok(PermissionsCommand::Mode(Some("acceptEdits".to_string())))
        );
        assert_eq!(
            parse("allow Bash(npm test *)"),
            Ok(PermissionsCommand::Add(
                RuleBehavior::Allow,
                "Bash(npm test *)".to_string()
            ))
        );
        assert_eq!(
            parse("deny   Read(./.env)"),
            Ok(PermissionsCommand::Add(
                RuleBehavior::Deny,
                "Read(./.env)".to_string()
            ))
        );
        assert_eq!(
            parse("remove WebSearch"),
            Ok(PermissionsCommand::Remove("WebSearch".to_string()))
        );
        assert_eq!(
            parse("check Bash(git push)"),
            Ok(PermissionsCommand::Check("Bash(git push)".to_string()))
        );
        assert!(parse("allow").is_err());
        assert!(parse("frobnicate").is_err());
    }
}
