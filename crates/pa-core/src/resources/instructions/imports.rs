//! `@path` imports in instruction files (Claude Code's CLAUDE.md imports):
//! which tokens name an import, and where each one resolves.
//!
//! A token is `@` at the start of a line or after whitespace, running to the
//! next unescaped whitespace (`\ ` keeps a space in the path), with any
//! `#fragment` dropped. Tokens inside fenced code blocks and inline code
//! spans are text, never imports.

use std::path::{Path, PathBuf};

/// How deep imports nest: a file imported by a file imported by ... the
/// top-level file stops importing past this many hops (Claude Code's cap).
pub const MAX_IMPORT_DEPTH: usize = 5;

/// The import targets `content` names, in order, outside code.
#[must_use]
pub fn import_targets(content: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some((marker, length)) = fence_marker(trimmed) {
            match fence {
                None => {
                    fence = Some((marker, length));
                    continue;
                }
                Some((open_marker, open_length)) => {
                    let rest = &trimmed[length * marker.len_utf8()..];
                    if marker == open_marker && length >= open_length && rest.trim().is_empty() {
                        fence = None;
                    }
                    continue;
                }
            }
        }
        if fence.is_some() {
            continue;
        }
        collect_tokens(&strip_inline_code(line), &mut targets);
    }
    targets
}

/// A fence opener or closer: three or more backticks or tildes.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = line.chars().take_while(|c| *c == marker).count();
    (length >= 3).then_some((marker, length))
}

/// The line with every closed code span blanked (as `CommonMark` reads it: a backtick
/// run closes at the next run of the same length; an unclosed run is text).
fn strip_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '`' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let run = chars[index..].iter().take_while(|c| **c == '`').count();
        let mut search = index + run;
        let mut close = None;
        while search < chars.len() {
            if chars[search] == '`' {
                let candidate = chars[search..].iter().take_while(|c| **c == '`').count();
                if candidate == run {
                    close = Some(search);
                    break;
                }
                search += candidate;
            } else {
                search += 1;
            }
        }
        if let Some(close) = close {
            out.push(' ');
            index = close + run;
        } else {
            out.extend(&chars[index..index + run]);
            index += run;
        }
    }
    out
}

fn collect_tokens(line: &str, targets: &mut Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let starts_token = chars[index] == '@' && (index == 0 || chars[index - 1].is_whitespace());
        if !starts_token {
            index += 1;
            continue;
        }
        let mut token = String::new();
        let mut cursor = index + 1;
        while cursor < chars.len() {
            match chars[cursor] {
                '\\' if chars.get(cursor + 1).is_some_and(|c| *c == ' ') => {
                    token.push(' ');
                    cursor += 2;
                }
                c if c.is_whitespace() => break,
                c => {
                    token.push(c);
                    cursor += 1;
                }
            }
        }
        index = cursor;
        if let Some(hash) = token.find('#') {
            token.truncate(hash);
        }
        // Sentence punctuation after a path is prose (`see @AGENTS.md.`).
        let kept = token
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
            .len();
        if kept > 0 && !token.ends_with("..") {
            token.truncate(kept);
        }
        if is_import_path(&token) {
            targets.push(token);
        }
    }
}

/// Whether a token reads as a path: `./x`, `~/x`, `/x`, or a bare name
/// starting with a path character (so `@mentions` of `@#`/`@@` are not).
fn is_import_path(token: &str) -> bool {
    if token.is_empty() || token == "/" {
        return false;
    }
    if token.starts_with("./") || token.starts_with("../") || token.starts_with("~/") {
        return true;
    }
    if token.starts_with('/') {
        return true;
    }
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Where an import names: `~/` under `home`, absolute as is, anything else
/// relative to the importing file's directory. `None` for `~/` without a home.
#[must_use]
pub fn resolve_import(target: &str, importing_file: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = target.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    let path = Path::new(target);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    Some(importing_file.parent().unwrap_or(Path::new("")).join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_path_form_and_drops_fragments() {
        let text = "See @AGENTS.md and @./docs/a.md, @~/notes.md.\n@/abs/b.md#section @dir/with\\ space.md";
        assert_eq!(
            import_targets(text),
            vec![
                "AGENTS.md",
                "./docs/a.md",
                "~/notes.md",
                "/abs/b.md",
                "dir/with space.md"
            ]
        );
    }

    #[test]
    fn ignores_emails_mentions_and_code() {
        let text = "\
mail me@example.com, ping @@team or @#chan
inline `@skip.md` but @keep.md
```
@fenced.md
```
~~~~ python
@tilde.md
~~~
still fenced @nope.md
~~~~
``@double.md`` and `unclosed @open.md";
        assert_eq!(import_targets(text), vec!["keep.md", "open.md"]);
    }

    #[test]
    fn resolves_relative_to_the_importing_file() {
        let home = Path::new("/home/u");
        let file = Path::new("/repo/sub/CLAUDE.md");
        assert_eq!(
            resolve_import("x.md", file, Some(home)),
            Some(PathBuf::from("/repo/sub/x.md"))
        );
        assert_eq!(
            resolve_import("../y.md", file, Some(home)),
            Some(PathBuf::from("/repo/sub/../y.md"))
        );
        assert_eq!(
            resolve_import("~/z.md", file, Some(home)),
            Some(PathBuf::from("/home/u/z.md"))
        );
        assert_eq!(resolve_import("~/z.md", file, None), None);
        assert_eq!(
            resolve_import("/abs.md", file, None),
            Some(PathBuf::from("/abs.md"))
        );
    }
}
