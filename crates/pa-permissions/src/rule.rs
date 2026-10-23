//! Permission rule strings in Claude Code's syntax: `Tool` (every use of a
//! tool) or `Tool(content)` (uses whose content matches), with `\(`, `\)` and
//! `\\` escapes inside the content. The parser, printer and validator are
//! ports of Claude Code 2.1.296's (`dpe`/`Un`/`_n` and `Lxe`): a rule string
//! always parses (the lenient reading the matcher uses), and validation
//! separately reports what Claude Code refuses or warns about.

use std::fmt;

/// What a matching rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuleBehavior {
    Allow,
    Ask,
    Deny,
}

impl RuleBehavior {
    pub const ALL: [RuleBehavior; 3] = [RuleBehavior::Allow, RuleBehavior::Ask, RuleBehavior::Deny];

    /// The settings key (`permissions.allow`, ...).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            RuleBehavior::Allow => "allow",
            RuleBehavior::Ask => "ask",
            RuleBehavior::Deny => "deny",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        RuleBehavior::ALL
            .into_iter()
            .find(|behavior| behavior.key() == text)
    }
}

/// One parsed rule: the tool it names and, for a scoped rule, its content
/// (unescaped). `Bash(*)` and `Bash()` read as the bare `Bash`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuleValue {
    pub tool: String,
    pub content: Option<String>,
}

/// Claude Code's legacy tool names and their current ones.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("Task", "Agent"),
    ("KillShell", "TaskStop"),
    ("KillBash", "TaskStop"),
    ("ListPeers", "ListAgents"),
    ("Brief", "SendUserMessage"),
];

/// The tools whose content is a command prefix or wildcard.
pub const BASH_TOOL: &str = "Bash";

/// The tools whose content is a gitignore-style path pattern.
const FILE_PATTERN_TOOLS: &[&str] = &["Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Glob"];

fn canonical_tool(name: &str) -> String {
    TOOL_ALIASES
        .iter()
        .find(|(legacy, _)| *legacy == name)
        .map_or(name, |(_, current)| current)
        .to_string()
}

/// The index of the first (`last == false`) or last unescaped `target` in
/// `text`: one preceded by an even run of backslashes.
fn unescaped(text: &str, target: char, last: bool) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let is_unescaped = |position: usize| {
        let backslashes = chars[..position]
            .iter()
            .rev()
            .take_while(|(_, ch)| *ch == '\\')
            .count();
        backslashes % 2 == 0
    };
    let mut indices = (0..chars.len()).filter(|&position| chars[position].1 == target);
    let found = if last {
        indices.rev().find(|&position| is_unescaped(position))
    } else {
        indices.find(|&position| is_unescaped(position))
    };
    found.map(|position| chars[position].0)
}

/// A rule string's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Bare,
    Malformed,
    Call { tool: String, raw_content: String },
}

fn shape(text: &str) -> Shape {
    if !text.contains(['(', ')']) {
        return Shape::Bare;
    }
    let (open, close) = (unescaped(text, '(', false), unescaped(text, ')', true));
    let (Some(open), Some(close)) = (open, close) else {
        return if open.is_none() && close.is_none() {
            Shape::Bare
        } else {
            Shape::Malformed
        };
    };
    let tool = &text[..open];
    if close <= open || close != text.len() - 1 || tool.contains(['(', ')']) {
        return Shape::Malformed;
    }
    Shape::Call {
        tool: tool.to_string(),
        raw_content: text[open + 1..close].to_string(),
    }
}

/// Undo the content's escapes: `\(` and `\)` first, then `\\`.
fn unescape(raw: &str) -> String {
    raw.replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

/// Escape content for printing: `\`, `(` and `)`.
fn escape(content: &str) -> String {
    content
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

impl RuleValue {
    /// Read a rule string the way the matcher does (never fails; see
    /// [`validate`] for what Claude Code refuses).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match shape(text) {
            Shape::Call { tool, raw_content } if !tool.is_empty() => {
                if raw_content.is_empty() || raw_content == "*" {
                    RuleValue {
                        tool: canonical_tool(&tool),
                        content: None,
                    }
                } else {
                    RuleValue {
                        tool: canonical_tool(&tool),
                        content: Some(unescape(&raw_content)),
                    }
                }
            }
            Shape::Bare | Shape::Malformed | Shape::Call { .. } => RuleValue {
                tool: canonical_tool(text),
                content: None,
            },
        }
    }

    /// A bare rule for `tool`.
    #[must_use]
    pub fn tool(tool: &str) -> Self {
        RuleValue {
            tool: tool.to_string(),
            content: None,
        }
    }

    /// A scoped rule.
    #[must_use]
    pub fn scoped(tool: &str, content: &str) -> Self {
        RuleValue {
            tool: tool.to_string(),
            content: Some(content.to_string()),
        }
    }

    /// The MCP server and tool a `mcp__<server>[__<tool>]` rule names.
    #[must_use]
    pub fn mcp(&self) -> Option<(&str, Option<&str>)> {
        parse_mcp_name(&self.tool)
    }
}

/// `mcp__<server>` or `mcp__<server>__<tool>`: the server and the tool
/// (`None` for every tool of the server, `*` included).
#[must_use]
pub fn parse_mcp_name(name: &str) -> Option<(&str, Option<&str>)> {
    let rest = name.strip_prefix("mcp__")?;
    if rest.is_empty() {
        return None;
    }
    match rest.split_once("__") {
        Some((server, tool)) if !server.is_empty() => {
            Some((server, (!tool.is_empty() && tool != "*").then_some(tool)))
        }
        Some(_) => None,
        None => Some((rest, None)),
    }
}

impl fmt::Display for RuleValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.content {
            Some(content) if !content.is_empty() => {
                write!(f, "{}({})", self.tool, escape(content))
            }
            _ => f.write_str(&self.tool),
        }
    }
}

/// What validating a rule string found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    Valid,
    /// Valid, with a warning about how it matches.
    Warning(String),
    /// Refused: the rule is skipped.
    Invalid {
        error: String,
        suggestion: Option<String>,
    },
}

impl Validation {
    fn invalid(error: &str, suggestion: Option<String>) -> Self {
        Validation::Invalid {
            error: error.to_string(),
            suggestion,
        }
    }
}

/// Whether `text` has a `*` not preceded by an odd run of backslashes.
pub(crate) fn has_unescaped_star(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.iter().enumerate().any(|(index, ch)| {
        *ch == '*'
            && chars[..index]
                .iter()
                .rev()
                .take_while(|ch| **ch == '\\')
                .count()
                % 2
                == 0
    })
}

/// `content`'s legacy prefix (`npm run test` for `npm run test:*`).
pub(crate) fn legacy_prefix(content: &str) -> Option<&str> {
    content
        .strip_suffix(":*")
        .filter(|prefix| !prefix.is_empty())
}

/// An allow rule's first word when a wildcard stands before the rest of the
/// command (`git * status`): it also approves options inserted there.
fn wildcard_before_rest(content: &str) -> Option<&str> {
    if content.ends_with(":*") {
        return None;
    }
    let words: Vec<&str> = content.split_whitespace().collect();
    let first = *words.first()?;
    if words.len() < 3 || has_unescaped_star(first) {
        return None;
    }
    let mut starred = false;
    for word in &words[1..] {
        if has_unescaped_star(word) {
            starred = true;
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        return starred.then_some(first);
    }
    None
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The shape errors: a malformed rule, empty parentheses.
fn validate_shape(shape: &Shape) -> Option<Validation> {
    match shape {
        Shape::Malformed => Some(Validation::invalid(
            "Malformed Tool(content) rule",
            Some(
                "Rules take the form Tool or Tool(content) and must end at the closing \")\"; \
                 parentheses inside the content are literal"
                    .to_string(),
            ),
        )),
        Shape::Call { tool, raw_content } if raw_content.is_empty() => Some(if tool.is_empty() {
            Validation::invalid(
                "Empty parentheses with no tool name",
                Some("Specify a tool name before the parentheses".to_string()),
            )
        } else {
            Validation::invalid(
                "Empty parentheses",
                Some(format!(
                    "Either specify a pattern or use just \"{tool}\" without parentheses"
                )),
            )
        }),
        Shape::Bare | Shape::Call { .. } => None,
    }
}

/// A Bash rule's errors and warnings: an empty prefix, a wildcard ahead of
/// an allowed command's rest, `:*` mixed with `*` or not at the end.
fn validate_bash(rule: &RuleValue, content: &str, behavior: RuleBehavior) -> Option<Validation> {
    if content == ":*" {
        return Some(Validation::invalid(
            "Prefix cannot be empty before :*",
            Some("Specify a command prefix before :*".to_string()),
        ));
    }
    if behavior == RuleBehavior::Allow {
        if let Some(word) = wildcard_before_rest(content) {
            let (git, example) = if word == "git" {
                (
                    " For git, options such as -c and --exec-path can run arbitrary commands.",
                    " (for example Bash(git status *))",
                )
            } else {
                ("", "")
            };
            return Some(Validation::Warning(format!(
                "{rule} has a wildcard before the rest of the command, so it also matches any \
                 options inserted at that position and approves them without a prompt.{git} \
                 Replace that * with the exact value you mean, or only use * after the \
                 subcommand{example}."
            )));
        }
    }
    if let Some(prefix) = legacy_prefix(content).map(str::trim_end) {
        if has_unescaped_star(prefix) {
            return Some(Validation::Warning(format!(
                "{rule} mixes * with the trailing :* prefix syntax, so it is matched as a literal \
                 prefix (the * is not expanded)."
            )));
        }
    } else if content.contains(":*") {
        return Some(Validation::Warning(format!(
            "{rule} has a :* that is not at the end, so it is matched as a * wildcard (the : is \
             literal), not as the trailing :* prefix syntax."
        )));
    }
    None
}

/// Validate `text` as a rule of `behavior`, with Claude Code's errors and
/// warnings.
#[must_use]
pub fn validate(text: &str, behavior: RuleBehavior) -> Validation {
    if text.trim().is_empty() {
        return Validation::invalid("Permission rule cannot be empty", None);
    }
    let shape = shape(text);
    if let Some(found) = validate_shape(&shape) {
        return found;
    }
    let rule = RuleValue::parse(text);
    if let Some((server, _)) = rule.mcp() {
        if matches!(shape, Shape::Call { .. }) {
            return Validation::invalid(
                "MCP rules do not support patterns in parentheses",
                Some(format!(
                    "Use \"{}\" without parentheses, or use \"mcp__{server}__*\" for all tools",
                    rule.tool
                )),
            );
        }
        return Validation::Valid;
    }
    if rule.tool.is_empty() {
        return Validation::invalid("Tool name cannot be empty", None);
    }
    let first = rule.tool.chars().next().unwrap_or_default();
    if !rule.tool.contains('_') && !first.is_uppercase() {
        return Validation::invalid(
            "Tool names must start with uppercase",
            Some(format!("Use \"{}\"", capitalized(&rule.tool))),
        );
    }
    let Some(content) = rule.content.as_deref() else {
        return Validation::Valid;
    };
    if rule.tool == BASH_TOOL {
        if let Some(found) = validate_bash(&rule, content, behavior) {
            return found;
        }
    }
    if FILE_PATTERN_TOOLS.contains(&rule.tool.as_str()) && content.contains(":*") {
        return Validation::invalid(
            "The \":*\" syntax is only for Bash prefix rules",
            Some("Use glob patterns like \"*\" or \"**\" for file matching".to_string()),
        );
    }
    let covered_by = match rule.tool.as_str() {
        "Write" | "NotebookEdit" | "MultiEdit" => Some("Edit"),
        "Glob" => Some("Read"),
        _ => None,
    };
    if let Some(covering) = covered_by {
        return Validation::Warning(format!(
            "{rule} is matched as {} (only {covering}(path) rules are file permission rules; \
             {covering} rules cover every file-{} action).",
            RuleValue::scoped(covering, content),
            if covering == "Edit" {
                "editing"
            } else {
                "reading"
            }
        ));
    }
    Validation::Valid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code's documented rule examples, parsed and printed back.
    #[test]
    fn claude_code_examples_parse_and_print_back() {
        let table: &[(&str, &str, Option<&str>)] = &[
            ("Bash", "Bash", None),
            ("Bash(*)", "Bash", None),
            ("Bash()", "Bash", None),
            ("Bash(git:*)", "Bash", Some("git:*")),
            ("Bash(npm run test:*)", "Bash", Some("npm run test:*")),
            ("Bash(npm run build)", "Bash", Some("npm run build")),
            ("Bash(git commit *)", "Bash", Some("git commit *")),
            ("Bash(* --version)", "Bash", Some("* --version")),
            ("Read(./secrets/**)", "Read", Some("./secrets/**")),
            ("Read(~/.zshrc)", "Read", Some("~/.zshrc")),
            ("Read(//etc/passwd)", "Read", Some("//etc/passwd")),
            ("Edit(src/**)", "Edit", Some("src/**")),
            ("Edit(/docs/**)", "Edit", Some("/docs/**")),
            ("Edit(**/*.env)", "Edit", Some("**/*.env")),
            (
                "WebFetch(domain:example.com)",
                "WebFetch",
                Some("domain:example.com"),
            ),
            ("WebSearch", "WebSearch", None),
            ("mcp__puppeteer", "mcp__puppeteer", None),
            (
                "mcp__puppeteer__puppeteer_navigate",
                "mcp__puppeteer__puppeteer_navigate",
                None,
            ),
            ("Task(Explore)", "Agent", Some("Explore")),
            (
                r"Bash(python -c print\(1\))",
                "Bash",
                Some("python -c print(1)"),
            ),
            (r"Bash(echo \\\(x)", "Bash", Some(r"echo \(x")),
            ("Bash(echo (x))", "Bash", Some("echo (x)")),
        ];
        for (text, tool, content) in table {
            let rule = RuleValue::parse(text);
            assert_eq!(
                rule,
                RuleValue {
                    tool: (*tool).to_string(),
                    content: content.map(str::to_string),
                },
                "{text}"
            );
            // Printing re-escapes, and the printed rule reads back the same.
            assert_eq!(RuleValue::parse(&rule.to_string()), rule, "{text}");
        }
        assert_eq!(
            RuleValue::parse("Bash(python -c print(1))").to_string(),
            r"Bash(python -c print\(1\))"
        );
    }

    #[test]
    fn malformed_rules_read_as_bare_tool_names() {
        for text in ["Bash(git", "Bash)x(", "Bash(x) y", r"Ba\(sh(x)"] {
            assert_eq!(RuleValue::parse(text).content, None, "{text}");
            assert!(
                matches!(
                    validate(text, RuleBehavior::Allow),
                    Validation::Invalid { .. }
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn mcp_names_split_into_server_and_tool() {
        assert_eq!(parse_mcp_name("mcp__github"), Some(("github", None)));
        assert_eq!(parse_mcp_name("mcp__github__*"), Some(("github", None)));
        assert_eq!(
            parse_mcp_name("mcp__github__get_issue"),
            Some(("github", Some("get_issue")))
        );
        assert_eq!(parse_mcp_name("mcp__"), None);
        assert_eq!(parse_mcp_name("Bash"), None);
    }

    fn error_of(text: &str, behavior: RuleBehavior) -> Option<String> {
        match validate(text, behavior) {
            Validation::Invalid { error, .. } => Some(error),
            Validation::Valid | Validation::Warning(_) => None,
        }
    }

    fn warns(text: &str, behavior: RuleBehavior) -> bool {
        matches!(validate(text, behavior), Validation::Warning(_))
    }

    /// Claude Code's validation errors, by message.
    #[test]
    fn validation_reports_claude_codes_errors() {
        let table: &[(&str, &str)] = &[
            ("", "Permission rule cannot be empty"),
            ("   ", "Permission rule cannot be empty"),
            ("Bash(git", "Malformed Tool(content) rule"),
            ("()", "Empty parentheses with no tool name"),
            ("Bash()", "Empty parentheses"),
            (
                "mcp__github(x)",
                "MCP rules do not support patterns in parentheses",
            ),
            ("bash(ls)", "Tool names must start with uppercase"),
            ("Bash(:*)", "Prefix cannot be empty before :*"),
            (
                "Read(src:*)",
                "The \":*\" syntax is only for Bash prefix rules",
            ),
        ];
        for (text, error) in table {
            assert_eq!(
                error_of(text, RuleBehavior::Deny).as_deref(),
                Some(*error),
                "{text:?}"
            );
        }
        for text in [
            "Bash",
            "Bash(git:*)",
            "Bash(npm run test *)",
            "Read(./.env)",
            "WebFetch(domain:example.com)",
            "mcp__github__*",
            "python_repl",
        ] {
            assert_eq!(
                validate(text, RuleBehavior::Allow),
                Validation::Valid,
                "{text}"
            );
        }
        assert_eq!(
            validate("bash", RuleBehavior::Deny),
            Validation::Invalid {
                error: "Tool names must start with uppercase".to_string(),
                suggestion: Some("Use \"Bash\"".to_string()),
            }
        );
    }

    #[test]
    fn validation_warns_like_claude_code() {
        // A wildcard ahead of the rest of an allowed command.
        assert!(warns("Bash(git * status)", RuleBehavior::Allow));
        assert!(!warns("Bash(git * status)", RuleBehavior::Deny));
        assert!(!warns("Bash(git status *)", RuleBehavior::Allow));
        // `:*` mixed with `*`, or not at the end.
        assert!(warns("Bash(npm * test:*)", RuleBehavior::Deny));
        assert!(warns("Bash(npm:* test)", RuleBehavior::Deny));
        // Write/Glob rules are matched as Edit/Read.
        assert!(warns("Write(src/**)", RuleBehavior::Deny));
        assert!(warns("Glob(src/**)", RuleBehavior::Deny));
    }
}
