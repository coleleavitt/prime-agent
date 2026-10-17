//! Claude Code's environment-variable expansion in MCP server configs:
//! `${VAR}` and `${VAR:-default}`, where `VAR` is `[A-Za-z_][A-Za-z0-9_]*`
//! and a default runs to the first `}`. A set variable expands to its value
//! (an empty value included); an unset one takes its default, or stays as
//! written and is reported missing. Anything else (`$VAR`, `${1}`, an
//! unclosed `${`) is literal text.

/// One expanded string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub text: String,
    /// Referenced variables that are unset and have no default, in order of
    /// first reference.
    pub missing: Vec<String>,
    /// Referenced variables read as empty because they never expand toward
    /// this destination (a provider credential in a remote server's URL or
    /// headers).
    pub blanked: Vec<String>,
}

/// One `${...}` reference at the start of `text`: the variable, its
/// default, and the reference's length in bytes.
fn reference(text: &str) -> Option<(&str, Option<&str>, usize)> {
    let body = text.strip_prefix("${")?;
    let name_len = body
        .char_indices()
        .take_while(|(index, char)| {
            char.is_ascii_alphabetic() || *char == '_' || (*index > 0 && char.is_ascii_digit())
        })
        .count();
    if name_len == 0 {
        return None;
    }
    let (name, rest) = body.split_at(name_len);
    if let Some(after) = rest.strip_prefix('}') {
        return Some((name, None, text.len() - after.len()));
    }
    let default_and_rest = rest.strip_prefix(":-")?;
    let close = default_and_rest.find('}')?;
    let default = &default_and_rest[..close];
    let consumed = text.len() - default_and_rest.len() + close + 1;
    Some((name, Some(default), consumed))
}

/// Expand the `${VAR}` / `${VAR:-default}` references in `text`. `lookup`
/// reads a variable; `blank` names the variables that read as empty
/// whatever their value (and win over a default).
pub fn expand_env_vars(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    blank: &dyn Fn(&str) -> bool,
) -> Expansion {
    let mut expanded = String::with_capacity(text.len());
    let mut missing: Vec<String> = Vec::new();
    let mut blanked: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let Some((name, default, consumed)) = reference(candidate) else {
            expanded.push_str("${");
            rest = &candidate[2..];
            continue;
        };
        if blank(name) {
            if !blanked.iter().any(|known| known == name) {
                blanked.push(name.to_string());
            }
        } else if let Some(value) = lookup(name) {
            expanded.push_str(&value);
        } else if let Some(default) = default {
            expanded.push_str(default);
        } else {
            if !missing.iter().any(|known| known == name) {
                missing.push(name.to_string());
            }
            expanded.push_str(&candidate[..consumed]);
        }
        rest = &candidate[consumed..];
    }
    expanded.push_str(rest);
    Expansion {
        text: expanded,
        missing,
        blanked,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn expand(text: &str) -> Expansion {
        let env: HashMap<&str, &str> =
            HashMap::from([("HOME", "/home/u"), ("EMPTY", ""), ("TOKEN", "t0k")]);
        expand_env_vars(
            text,
            &|name| env.get(name).map(|value| (*value).to_string()),
            &|name| name == "SECRET",
        )
    }

    #[test]
    fn set_variables_expand_and_unset_ones_take_their_default() {
        let expansion = expand("${HOME}/bin:${UNSET:-/opt/bin}:${EMPTY:-fallback}");
        assert_eq!(
            expansion,
            Expansion {
                text: "/home/u/bin:/opt/bin:".to_string(),
                missing: Vec::new(),
                blanked: Vec::new(),
            }
        );
    }

    #[test]
    fn a_missing_variable_stays_as_written_and_is_reported_once() {
        let expansion = expand("a ${NOPE} b ${NOPE} c ${ALSO_NOPE}");
        assert_eq!(
            expansion,
            Expansion {
                text: "a ${NOPE} b ${NOPE} c ${ALSO_NOPE}".to_string(),
                missing: vec!["NOPE".to_string(), "ALSO_NOPE".to_string()],
                blanked: Vec::new(),
            }
        );
    }

    #[test]
    fn non_references_are_literal_text() {
        for text in [
            "$HOME",
            "${1ABC}",
            "${}",
            "${HOME",
            "${HOME:-unclosed",
            "${HO-ME}",
            "$${",
            "price: $5",
        ] {
            assert_eq!(expand(text).text, text, "{text}");
        }
        assert_eq!(expand("$${HOME}").text, "$/home/u");
    }

    #[test]
    fn a_default_runs_to_the_first_brace_and_may_be_empty() {
        assert_eq!(expand("${UNSET:-}").text, "");
        assert_eq!(expand("${UNSET:-a:-b}x}").text, "a:-bx}");
        assert_eq!(expand("Bearer ${TOKEN:-none}").text, "Bearer t0k");
    }

    #[test]
    fn blanked_variables_read_empty_over_a_value_or_default() {
        let expansion = expand("k=${SECRET:-default}&t=${TOKEN}");
        assert_eq!(
            expansion,
            Expansion {
                text: "k=&t=t0k".to_string(),
                missing: Vec::new(),
                blanked: vec!["SECRET".to_string()],
            }
        );
    }

    #[test]
    fn multibyte_text_around_references_survives() {
        assert_eq!(expand("é${HOME}ü${X:-ß}").text, "é/home/uüß");
    }
}
