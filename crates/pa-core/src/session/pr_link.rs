//! A session's pull-request link (Claude Code's `pr-link` transcript entry): when a command the
//! session ran creates a pull or merge request (`gh pr create`, `glab mr create`) and prints its
//! URL, the session records a `custom` entry of type [`PR_LINK_CUSTOM_TYPE`]; the saved-session
//! catalog folds the newest one into the row's `prNumber`/`prUrl`/`prRepository`, which the
//! resume picker shows, searches and filters on (`--from-pr`).

use serde::{Deserialize, Serialize};

/// The `custom` entry `customType` of a pull-request link; `data` is a [`SessionPrLink`].
pub const PR_LINK_CUSTOM_TYPE: &str = "pr_link";

/// One pull or merge request a session is linked to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPrLink {
    #[serde(rename = "prNumber")]
    pub number: u64,
    #[serde(rename = "prUrl")]
    pub url: String,
    /// `owner/repo` (GitLab: the project path).
    #[serde(rename = "prRepository")]
    pub repository: String,
}

/// Whether `command` creates a pull or merge request (Claude Code's `zT`: a `gh pr create` or
/// `glab mr create` that is not a `--dry-run`).
#[must_use]
pub fn creates_pull_request(command: &str) -> bool {
    if command.contains("--dry-run") {
        return false;
    }
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .collect();
    words.windows(3).any(|window| {
        matches!(window, ["gh", "pr", "create"] | ["glab", "mr", "create"])
            || matches!(window, [tool, "pr" | "mr", "create"] if tool.ends_with("/gh") || tool.ends_with("/glab"))
    })
}

/// The last pull or merge request URL in `text` (Claude Code's `uqe`): a GitHub
/// `https://<host>/<owner>/<repo>/pull/<n>`, a GitLab `.../<project>/-/merge_requests/<n>`, or a
/// Bitbucket `.../<owner>/<repo>/pull-requests/<n>` (Bitbucket Server's
/// `/projects/<p>/repos/<r>/pull-requests/<n>` too).
#[must_use]
pub fn find_pr_link(text: &str) -> Option<SessionPrLink> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')'))
        .filter_map(parse_pr_url)
        .next_back()
}

fn parse_pr_url(token: &str) -> Option<SessionPrLink> {
    let rest = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))?;
    let segments: Vec<&str> = rest.split('/').collect();
    let host = *segments.first().filter(|host| !host.is_empty())?;
    let marker = segments
        .iter()
        .position(|segment| matches!(*segment, "pull" | "pull-requests" | "merge_requests"))?;
    let digits: String = segments
        .get(marker + 1)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let number = digits.parse::<u64>().ok().filter(|number| *number > 0)?;
    let mut path: Vec<&str> = segments[1..marker].to_vec();
    if segments[marker] == "merge_requests" {
        // GitLab's `/-/` separator.
        if path.last() != Some(&"-") {
            return None;
        }
        path.pop();
    }
    if let ["projects", project, "repos", repo] = path.as_slice() {
        path = vec![project, repo];
    }
    if path.len() < 2 || path.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let repository = path.join("/");
    Some(SessionPrLink {
        number,
        url: format!(
            "https://{host}/{repository_path}/{marker}/{number}",
            repository_path = segments[1..marker].join("/"),
            marker = segments[marker],
        ),
        repository,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_commands_are_gh_pr_create_and_glab_mr_create() {
        assert!(creates_pull_request("gh pr create --fill"));
        assert!(creates_pull_request(
            "git push -u origin topic && gh pr create --title x --body y"
        ));
        assert!(creates_pull_request("glab mr create --fill"));
        assert!(creates_pull_request("/usr/bin/gh pr create"));
        assert!(!creates_pull_request("gh pr create --dry-run"));
        assert!(!creates_pull_request("gh pr view 12"));
        assert!(!creates_pull_request("echo gh pr"));
    }

    #[test]
    fn finds_the_last_pull_request_url_of_each_forge() {
        let github = find_pr_link(
            "Creating pull request for topic into main in acme/app\n\nhttps://github.com/acme/app/pull/42\n",
        );
        assert_eq!(
            github,
            Some(SessionPrLink {
                number: 42,
                url: "https://github.com/acme/app/pull/42".to_string(),
                repository: "acme/app".to_string(),
            })
        );
        assert_eq!(
            find_pr_link("see https://gitlab.com/group/sub/proj/-/merge_requests/7."),
            Some(SessionPrLink {
                number: 7,
                url: "https://gitlab.com/group/sub/proj/-/merge_requests/7".to_string(),
                repository: "group/sub/proj".to_string(),
            })
        );
        assert_eq!(
            find_pr_link("https://bitbucket.example/projects/P/repos/r/pull-requests/3/overview")
                .map(|link| (link.number, link.repository)),
            Some((3, "P/r".to_string()))
        );
        assert_eq!(
            find_pr_link("https://github.com/a/b/pull/1 then https://github.com/a/b/pull/2")
                .map(|link| link.number),
            Some(2)
        );
        assert_eq!(find_pr_link("https://github.com/acme/app/pulls"), None);
        assert_eq!(find_pr_link("https://github.com/pull/4"), None);
        assert_eq!(find_pr_link("no link here"), None);
    }

    #[test]
    fn the_link_serializes_as_claude_codes_fields() {
        let link = SessionPrLink {
            number: 5,
            url: "https://github.com/a/b/pull/5".to_string(),
            repository: "a/b".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&link).unwrap(),
            serde_json::json!({
                "prNumber": 5,
                "prUrl": "https://github.com/a/b/pull/5",
                "prRepository": "a/b",
            })
        );
    }
}
