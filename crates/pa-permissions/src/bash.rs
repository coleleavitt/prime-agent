//! Bash rule content, Claude Code's three kinds (`Dxe`): a legacy prefix
//! (`npm run test:*`: the command is the prefix or starts with it and a
//! space), a wildcard (an unescaped `*` matches any run of characters; a
//! single trailing ` *` also matches the bare command, so `git status *`
//! covers `git status`), or an exact command. Matching normalizes runs of
//! spaces and tabs, like Claude Code's.

use regex::Regex;

use crate::rule::{has_unescaped_star, legacy_prefix};

/// One Bash rule's content, compiled.
#[derive(Debug, Clone)]
pub enum BashPattern {
    Exact(String),
    Prefix(String),
    Wildcard { pattern: String, regex: Regex },
}

/// Runs of spaces and tabs as one space.
fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch == ' ' || ch == '\t' {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

/// A pattern piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Literal(char),
    Star,
}

/// Compile a wildcard pattern the way Claude Code's `oZ` does: `\*` is a
/// literal star and `\\` a literal backslash; `*` matches anything
/// (newlines included); `/**/` also matches `/`; a pattern whose only star
/// is a trailing ` *` also matches without the space and the rest.
fn wildcard_regex(pattern: &str) -> Option<Regex> {
    if pattern.contains('\0') {
        return None;
    }
    let normalized = normalize_whitespace(pattern.trim());
    let chars: Vec<char> = normalized.chars().collect();
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' && index + 1 < chars.len() && matches!(chars[index + 1], '*' | '\\') {
            pieces.push(Piece::Literal(chars[index + 1]));
            index += 2;
            continue;
        }
        pieces.push(if ch == '*' {
            Piece::Star
        } else {
            Piece::Literal(ch)
        });
        index += 1;
    }
    let stars = pieces.iter().filter(|piece| **piece == Piece::Star).count();
    let mut regex = String::from("(?s)^");
    let mut index = 0;
    while index < pieces.len() {
        // `/**/` (repeated): any run of directories, or none.
        if pieces[index] == Piece::Literal('/') {
            let mut end = index + 1;
            while pieces.get(end) == Some(&Piece::Star)
                && pieces.get(end + 1) == Some(&Piece::Star)
                && pieces.get(end + 2) == Some(&Piece::Literal('/'))
            {
                end += 3;
            }
            if end > index + 1 {
                regex.push_str("/(?:.*/)?");
                index = end;
                continue;
            }
        }
        let trailing_space_star = stars == 1
            && index + 2 == pieces.len()
            && pieces[index] == Piece::Literal(' ')
            && pieces[index + 1] == Piece::Star;
        if trailing_space_star {
            regex.push_str("( .*)?");
            break;
        }
        match pieces[index] {
            Piece::Star => regex.push_str(".*"),
            Piece::Literal(ch) => regex.push_str(&regex::escape(&ch.to_string())),
        }
        index += 1;
    }
    regex.push('$');
    Regex::new(&regex).ok()
}

impl BashPattern {
    /// Compile a Bash rule's content.
    #[must_use]
    pub fn new(content: &str) -> Self {
        if let Some(prefix) = legacy_prefix(content) {
            return BashPattern::Prefix(prefix.to_string());
        }
        if has_unescaped_star(content) {
            if let Some(regex) = wildcard_regex(content) {
                return BashPattern::Wildcard {
                    pattern: content.to_string(),
                    regex,
                };
            }
        }
        BashPattern::Exact(content.to_string())
    }

    /// Whether the pattern matches one simple command's text. `xargs`
    /// widens it to the command run through `xargs` (Claude Code does for
    /// prefixes always, for wildcards on deny and ask rules).
    #[must_use]
    pub fn matches(&self, command: &str, xargs: bool) -> bool {
        match self {
            BashPattern::Exact(exact) => exact == command,
            BashPattern::Prefix(prefix) => {
                let prefix = normalize_whitespace(prefix);
                let command = normalize_whitespace(command);
                let starts =
                    |with: &str| command == with || command.starts_with(&format!("{with} "));
                starts(&prefix) || starts(&format!("xargs {prefix}"))
            }
            BashPattern::Wildcard { pattern, regex } => {
                let command = normalize_whitespace(command);
                if regex.is_match(&command) {
                    return true;
                }
                xargs
                    && wildcard_regex(&format!("xargs {pattern}"))
                        .is_some_and(|regex| regex.is_match(&command))
            }
        }
    }

    /// Whether the pattern may approve a part of a compound command (an
    /// exact pattern names a whole command line).
    #[must_use]
    pub fn is_exact(&self) -> bool {
        matches!(self, BashPattern::Exact(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(content: &str, command: &str) -> bool {
        BashPattern::new(content).matches(command, false)
    }

    /// Claude Code's documented Bash rule examples.
    #[test]
    fn bash_rules_match_like_claude_code() {
        let table: &[(&str, &str, bool)] = &[
            // Legacy prefixes.
            ("npm run test:*", "npm run test", true),
            ("npm run test:*", "npm run test -- --watch", true),
            ("npm run test:*", "npm run testing", false),
            ("git:*", "git push origin main", true),
            ("git:*", "gitk", false),
            ("git  diff:*", "git diff  HEAD", true),
            ("ls:*", "xargs ls -la", true),
            // Wildcards.
            ("npm run *", "npm run build", true),
            ("npm run *", "npm run", true),
            ("ls *", "ls", true),
            ("ls *", "lsof", false),
            ("git * main", "git push origin main", true),
            ("git * main", "git push origin dev", false),
            ("* --version", "node --version", true),
            ("* --version", "node --help", false),
            ("git commit *", "git commit -m \"a\nb\"", true),
            // `\*` is a literal star; without an unescaped one the rule is
            // exact (Claude Code keeps the backslash).
            (r"echo \* *", "echo * hi", true),
            (r"echo \* *", "echo x hi", false),
            (r"echo \*", r"echo \*", true),
            (r"echo \*", "echo *", false),
            ("cat src/**/x.rs", "cat src/x.rs", true),
            ("cat src/**/x.rs", "cat src/a/b/x.rs", true),
            ("ls  *", "ls\t-la", true),
            // Exact.
            ("npm run build", "npm run build", true),
            ("npm run build", "npm run build --watch", false),
        ];
        for (content, command, expected) in table {
            assert_eq!(
                matches(content, command),
                *expected,
                "{content} vs {command}"
            );
        }
    }

    #[test]
    fn wildcards_reach_through_xargs_only_when_asked() {
        let pattern = BashPattern::new("rm *");
        assert!(!pattern.matches("xargs rm -f", false));
        assert!(pattern.matches("xargs rm -f", true));
    }

    #[test]
    fn a_trailing_star_after_another_star_is_not_optional() {
        // Two stars: the trailing ` *` needs its space and argument.
        assert!(!matches("git * *", "git status"));
        assert!(matches("git * *", "git status x"));
    }
}
