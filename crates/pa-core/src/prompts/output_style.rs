//! Output styles (Claude Code's `outputStyle`): a named prompt that changes
//! how the agent responds. The built-ins are Claude Code 2.1.296's
//! (`Proactive`, `Concise`, `Explanatory`, `Learning`); custom styles are
//! markdown files in `~/.claude/output-styles/`, the agent dir's
//! `output-styles/`, and (in a trusted workspace) the project's
//! `.claude/output-styles/` and `.prime/agent/output-styles/`.
//!
//! The selected style is a system-prompt input: its text is the
//! `output-style` segment (see [`output_style_section`]), and a style with
//! `keep-coding-instructions: false` (the default for custom styles) drops
//! the opinionated coding-guidelines layer. A mid-session switch rides the
//! next turn as a hidden row ([`output_style_change_text`]), the way Claude
//! Code delivers it, so the cached prefix never changes under a session.

use std::path::{Path, PathBuf};

use crate::skills::frontmatter::parse_frontmatter;

/// The settings value (and listing name) of the default style.
pub const DEFAULT_OUTPUT_STYLE: &str = "default";
/// The custom styles directory name under each config dir.
pub const OUTPUT_STYLES_DIR: &str = "output-styles";

/// Where a style came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyleSource {
    BuiltIn,
    User,
    Project,
}

/// One output style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub source: OutputStyleSource,
    /// Whether the coding guidelines stay in the system prompt (built-ins:
    /// yes; custom styles: only with `keep-coding-instructions: true`).
    pub keep_coding_instructions: bool,
    /// Claude Code's short per-turn reminder for the style, when it has one
    /// (a reminder producer may restate it each turn).
    pub turn_reminder: Option<String>,
}

const INSIGHTS: &str = "\n## Insights\nIn order to encourage learning, before and after writing code, always provide brief educational explanations about implementation choices using (with backticks):\n\"`\u{2605} Insight \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}`\n[2-3 key educational points]\n`\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}`\"\n\nThese insights should be included in the conversation, not in the codebase. You should generally focus on interesting insights that are specific to the codebase or the code you just wrote, rather than general programming concepts.";

const PROACTIVE_RULES: &str = "The user chose continuous, autonomous execution. You should:\n\n1. **Execute immediately** \u{2014} Start implementing right away. Make reasonable assumptions and proceed on low-risk work.\n2. **Minimize interruptions** \u{2014} Prefer making reasonable assumptions over asking questions for routine decisions.\n3. **Prefer action over planning** \u{2014} Do not enter plan mode unless the user explicitly asks. When in doubt, start coding.\n4. **Expect course corrections** \u{2014} The user may provide suggestions or course corrections at any point; treat those as normal input.\n5. **Do not take overly destructive actions** \u{2014} This is not a license to destroy. Anything that deletes data or modifies shared or production systems still needs explicit user confirmation. If you reach such a decision point, ask and wait, or course correct to a safer method instead.\n6. **Avoid data exfiltration** \u{2014} Post even routine messages to chat platforms or work tickets only if the user has directed you to. You must not share secrets (e.g. credentials, internal documentation) unless the user has explicitly authorized both that specific secret and its destination.";

const CONCISE_RULES: &str = "The user chose brevity over narration. You should:\n\n1. **Lead with the result** \u{2014} Your first sentence answers \"what happened\" or \"what's the answer.\" No preamble (\"Let me...\", \"Now I'll...\") and no closing recap of what you already said.\n2. **Cut narration, keep substance** \u{2014} Don't restate the request, the plan, or each step you took. Report outcomes, decisions, and anything the user must act on.\n3. **Short by default** \u{2014} Answer simple questions in 1-3 sentences of plain prose. Use headers, tables, and bullet lists only when they carry real structure, never as decoration.\n4. **State things plainly** \u{2014} Skip hedging boilerplate. Mention a caveat only when it changes what the user should do next.\n5. **Give full detail on request** \u{2014} When the user asks for an explanation or detail, answer completely. Conciseness never means withholding requested information.\n6. **Never trade correctness for brevity** \u{2014} Error reports, failing test output, security warnings, and confirmations for destructive actions keep their full content.\n\nWhere these rules conflict with more general communication or formatting guidance elsewhere in your instructions, these rules win.";

const LEARNING_BODY: &str = "## Requesting Human Contributions\nIn order to encourage learning, ask the human to contribute 2-10 line code pieces when generating 20+ lines involving:\n- Design decisions (error handling, data structures)\n- Business logic with multiple valid approaches  \n- Key algorithms or interface definitions\n\n**TodoList Integration**: If using a TodoList for the overall task, include a specific todo item like \"Request human input on [specific decision]\" when planning to request human input. This ensures proper task tracking. Note: TodoList is not required for all tasks.\n\nExample TodoList flow:\n   \u{2713} \"Set up component structure with placeholder for logic\"\n   \u{2713} \"Request human collaboration on decision logic implementation\"\n   \u{2713} \"Integrate contribution and complete feature\"\n\n### Request Format\n```\n\u{25cf} **Learn by Doing**\n**Context:** [what's built and why this decision matters]\n**Your Task:** [specific function/section in file, mention file and TODO(human) but do not include line numbers]\n**Guidance:** [trade-offs and constraints to consider]\n```\n\n### Key Guidelines\n- Frame contributions as valuable design decisions, not busy work\n- You must first add a TODO(human) section into the codebase with your editing tools before making the Learn by Doing request      \n- Make sure there is one and only one TODO(human) section in the code\n- Don't take any action or output anything after the Learn by Doing request. Wait for human implementation before proceeding.\n\n### Example Requests\n\n**Whole Function Example:**\n```\n\u{25cf} **Learn by Doing**\n\n**Context:** I've set up the hint feature UI with a button that triggers the hint system. The infrastructure is ready: when clicked, it calls selectHintCell() to determine which cell to hint, then highlights that cell with a yellow background and shows possible values. The hint system needs to decide which empty cell would be most helpful to reveal to the user.\n\n**Your Task:** In sudoku.js, implement the selectHintCell(board) function. Look for TODO(human). This function should analyze the board and return {row, col} for the best cell to hint, or null if the puzzle is complete.\n\n**Guidance:** Consider multiple strategies: prioritize cells with only one possible value (naked singles), or cells that appear in rows/columns/boxes with many filled cells. You could also consider a balanced approach that helps without making it too easy. The board parameter is a 9x9 array where 0 represents empty cells.\n```\n\n**Partial Function Example:**\n```\n\u{25cf} **Learn by Doing**\n\n**Context:** I've built a file upload component that validates files before accepting them. The main validation logic is complete, but it needs specific handling for different file type categories in the switch statement.\n\n**Your Task:** In upload.js, inside the validateFile() function's switch statement, implement the 'case \"document\":' branch. Look for TODO(human). This should validate document files (pdf, doc, docx).\n\n**Guidance:** Consider checking file size limits (maybe 10MB for documents?), validating the file extension matches the MIME type, and returning {valid: boolean, error?: string}. The file object has properties: name, size, type.\n```\n\n**Debugging Example:**\n```\n\u{25cf} **Learn by Doing**\n\n**Context:** The user reported that number inputs aren't working correctly in the calculator. I've identified the handleInput() function as the likely source, but need to understand what values are being processed.\n\n**Your Task:** In calculator.js, inside the handleInput() function, add 2-3 console.log statements after the TODO(human) comment to help debug why number inputs fail.\n\n**Guidance:** Consider logging: the raw input value, the parsed result, and any validation state. This will help us understand where the conversion breaks.\n```\n\n### After Contributions\nShare one insight connecting their code to broader patterns or system effects. Avoid praise or repetition.\n\n## Insights\n";

fn built_in(
    name: &str,
    description: &str,
    prompt: String,
    turn_reminder: Option<&str>,
) -> OutputStyle {
    OutputStyle {
        name: name.to_string(),
        description: description.to_string(),
        prompt,
        source: OutputStyleSource::BuiltIn,
        keep_coding_instructions: true,
        turn_reminder: turn_reminder.map(str::to_string),
    }
}

/// The built-in styles, in Claude Code's listing order (after `default`).
#[must_use]
pub fn builtin_output_styles() -> Vec<OutputStyle> {
    vec![
        built_in(
            "Proactive",
            "Claude executes immediately, minimizes interruptions, and prefers action over planning",
            format!(
                "You are an interactive CLI tool that helps users with software engineering tasks. You should work proactively and autonomously, executing immediately and minimizing interruptions.\n\n# Proactive Style Active\n{PROACTIVE_RULES}"
            ),
            Some("Execute autonomously, minimize interruptions, prefer action over planning."),
        ),
        built_in(
            "Concise",
            "Claude responds tersely, leading with results and skipping preamble and narration",
            format!(
                "You are an interactive CLI tool that helps users with software engineering tasks. Keep your responses short and direct while doing the work just as thoroughly.\n\n# Concise Style Active\n{CONCISE_RULES}"
            ),
            Some(
                "Be concise: lead with the result, skip preamble and narration, keep only what the user needs.",
            ),
        ),
        built_in(
            "Explanatory",
            "Claude explains its implementation choices and codebase patterns",
            format!(
                "You are an interactive CLI tool that helps users with software engineering tasks. In addition to software engineering tasks, you should provide educational insights about the codebase along the way.\n\nYou should be clear and educational, providing helpful explanations while remaining focused on the task. Balance educational content with task completion. When providing insights, you may exceed typical length constraints, but remain focused and relevant.\n\n# Explanatory Style Active\n{INSIGHTS}"
            ),
            None,
        ),
        built_in(
            "Learning",
            "Claude pauses and asks you to write small pieces of code for hands-on practice",
            format!(
                "You are an interactive CLI tool that helps users with software engineering tasks. In addition to software engineering tasks, you should help users learn more about the codebase through hands-on practice and educational insights.\n\nYou should be collaborative and encouraging. Balance task completion with learning by requesting user input for meaningful design decisions while handling routine implementation yourself.   \n\n# Learning Style Active\n{LEARNING_BODY}{INSIGHTS}"
            ),
            None,
        ),
    ]
}

/// The style directories, user scope first, then the project's (only when
/// the workspace is trusted: a style rewrites the system prompt).
#[must_use]
pub fn output_style_dirs(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
) -> Vec<(PathBuf, OutputStyleSource)> {
    let mut dirs = Vec::new();
    if let Some(claude) = crate::skills::claude_commands::user_claude_dir() {
        dirs.push((claude.join(OUTPUT_STYLES_DIR), OutputStyleSource::User));
    }
    dirs.push((agent_dir.join(OUTPUT_STYLES_DIR), OutputStyleSource::User));
    if project_trusted {
        dirs.push((
            cwd.join(crate::skills::claude_commands::CLAUDE_DIR_NAME)
                .join(OUTPUT_STYLES_DIR),
            OutputStyleSource::Project,
        ));
        dirs.push((
            cwd.join(crate::settings::CONFIG_DIR_NAME)
                .join(OUTPUT_STYLES_DIR),
            OutputStyleSource::Project,
        ));
    }
    dirs
}

fn truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(text)) => text.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn load_style_file(path: &Path, source: OutputStyleSource) -> Option<OutputStyle> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = parse_frontmatter(&raw);
    let stem = path
        .file_name()?
        .to_string_lossy()
        .trim_end_matches(".md")
        .to_string();
    let text = |key: &str| {
        frontmatter
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let name = text("name").unwrap_or_else(|| stem.clone());
    let description = text("description").unwrap_or_else(|| {
        body.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(
                || format!("Custom {stem} output style"),
                |line| {
                    let line = line.trim_start_matches('#').trim();
                    let chars: Vec<char> = line.chars().collect();
                    if chars.len() > 100 {
                        chars[..97].iter().collect::<String>() + "..."
                    } else {
                        line.to_string()
                    }
                },
            )
    });
    Some(OutputStyle {
        name,
        description,
        prompt: body.trim().to_string(),
        source,
        keep_coding_instructions: truthy(frontmatter.get("keep-coding-instructions")),
        turn_reminder: None,
    })
}

/// Every available style: the built-ins, then each directory's custom
/// styles in order; a later style replaces an earlier one of the same name
/// (project over user over built-in, as in Claude Code).
#[must_use]
pub fn load_output_styles(dirs: &[(PathBuf, OutputStyleSource)]) -> Vec<OutputStyle> {
    let mut styles = builtin_output_styles();
    for (dir, source) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.file_name().is_some_and(|name| {
                        crate::skills::skill_markdown_name(&name.to_string_lossy())
                    })
            })
            .collect();
        files.sort();
        for file in files {
            let Some(style) = load_style_file(&file, *source) else {
                continue;
            };
            match styles.iter_mut().find(|known| known.name == style.name) {
                Some(known) => *known = style,
                None => styles.push(style),
            }
        }
    }
    styles
}

/// The style a setting names: an exact name, else a case-insensitive one;
/// `None` for the default style or an unknown name.
#[must_use]
pub fn find_output_style<'a>(styles: &'a [OutputStyle], name: &str) -> Option<&'a OutputStyle> {
    let name = name.trim();
    styles.iter().find(|style| style.name == name).or_else(|| {
        styles
            .iter()
            .find(|style| style.name.eq_ignore_ascii_case(name))
    })
}

/// The system-prompt segment a style contributes (Claude Code's
/// `# Output Style: <name>` block).
#[must_use]
pub fn output_style_section(style: &OutputStyle) -> String {
    format!("# Output Style: {}\n{}", style.name, style.prompt)
}

/// The hidden row a mid-session switch delivers: the new style's block, or
/// Claude Code's reset notice for the default.
#[must_use]
pub fn output_style_change_text(style: Option<&OutputStyle>) -> String {
    match style {
        Some(style) => output_style_section(style),
        None => {
            "The output style was reset to the default. Respond in your usual style.".to_string()
        }
    }
}

/// `/output-style` without a known style: the current style and the list.
#[must_use]
pub fn output_style_listing(styles: &[OutputStyle], current: Option<&str>) -> String {
    let current = current.unwrap_or(DEFAULT_OUTPUT_STYLE);
    let marker = |name: &str| if name == current { " (current)" } else { "" };
    let mut lines = vec![format!(
        "- {DEFAULT_OUTPUT_STYLE}{}",
        marker(DEFAULT_OUTPUT_STYLE)
    )];
    lines.extend(styles.iter().map(|style| {
        format!(
            "- {}{}: {}",
            style.name,
            marker(&style.name),
            style.description
        )
    }));
    format!(
        "Output style: {current}\n\nAvailable styles:\n{}\n\nUsage: /output-style <style>",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_styles_load_override_and_default_to_dropping_coding_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            user.join("pirate.md"),
            "---\ndescription: Talk like a pirate\n---\nArr, answer as a pirate.\n",
        )
        .unwrap();
        std::fs::write(
            project.join("pirate.md"),
            "---\ndescription: Project pirate\nkeep-coding-instructions: true\n---\nProject arr.\n",
        )
        .unwrap();
        std::fs::write(project.join("terse.md"), "# Be terse\nShort answers.").unwrap();
        let styles = load_output_styles(&[
            (user, OutputStyleSource::User),
            (project, OutputStyleSource::Project),
        ]);
        let names: Vec<&str> = styles.iter().map(|style| style.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Proactive",
                "Concise",
                "Explanatory",
                "Learning",
                "pirate",
                "terse"
            ]
        );
        assert_eq!(
            find_output_style(&styles, "PIRATE"),
            Some(&OutputStyle {
                name: "pirate".to_string(),
                description: "Project pirate".to_string(),
                prompt: "Project arr.".to_string(),
                source: OutputStyleSource::Project,
                keep_coding_instructions: true,
                turn_reminder: None,
            })
        );
        let terse = find_output_style(&styles, "terse").unwrap();
        assert_eq!(
            (terse.description.as_str(), terse.keep_coding_instructions),
            ("Be terse", false)
        );
    }

    #[test]
    fn listing_marks_the_current_style() {
        let styles = vec![built_in("Explanatory", "Explains", String::new(), None)];
        assert_eq!(
            output_style_listing(&styles, Some("Explanatory")),
            "Output style: Explanatory\n\nAvailable styles:\n- default\n- Explanatory (current): Explains\n\nUsage: /output-style <style>"
        );
        assert!(output_style_listing(&styles, None).contains("- default (current)"));
    }
}
