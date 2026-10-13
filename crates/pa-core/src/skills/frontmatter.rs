//! YAML frontmatter parsing.

use serde_json::Value;

fn normalize_newlines(value: &str) -> String {
    value
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn extract_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = normalize_newlines(content);
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end_index) = normalized[3..].find("\n---") else {
        return (None, normalized);
    };
    // end_index is relative to offset 3; the yaml block runs from offset 4 to the absolute close,
    // the body starts after "\n---" (4 chars from the find position). An EMPTY block (the close
    // follows the open, e.g. `---\n---`) is NO frontmatter on the TS side (`!yamlString`): fall
    // back to the whole document instead of slicing.
    let Some(yaml) = normalized.get(4..end_index + 3) else {
        return (None, normalized);
    };
    if yaml.is_empty() {
        return (None, normalized);
    }
    let body = normalized[end_index + 3 + 4..].trim().to_string();
    (Some(yaml.to_string()), body)
}

/// YAML 1.2 core resolution, as the TS `yaml` reader resolves frontmatter: only `true`/`false`
/// are booleans (YAML 1.1 `yes`/`no`/`on`/`off` stay strings) and `<<` is an ordinary key.
fn yaml_options() -> serde_saphyr::Options {
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    options.merge_keys = serde_saphyr::MergeKeyPolicy::AsOrdinary;
    options
}

/// Parse `---` YAML frontmatter into a JSON object plus the body.
#[must_use]
pub fn parse_frontmatter(content: &str) -> (Value, String) {
    let (yaml_string, body) = extract_frontmatter(content);
    let Some(yaml_string) = yaml_string else {
        return (serde_json::Map::new().into(), body);
    };
    // A block that is not valid YAML, or whose mapping keys are not strings, is no frontmatter.
    let converted: Value = match serde_saphyr::from_str_with_options(&yaml_string, yaml_options()) {
        Ok(parsed) => parsed,
        Err(_) => return (serde_json::Map::new().into(), body),
    };
    let frontmatter = if converted.is_null() {
        serde_json::Map::new().into()
    } else {
        converted
    };
    (frontmatter, body)
}

/// The body with frontmatter stripped.
#[must_use]
pub fn strip_frontmatter(content: &str) -> String {
    parse_frontmatter(content).1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_frontmatter() {
        let (frontmatter, body) =
            parse_frontmatter("---\nname: my-skill\ndescription: A skill\n---\nUse it well.");
        assert_eq!(frontmatter["name"], "my-skill");
        assert_eq!(frontmatter["description"], "A skill");
        assert_eq!(body, "Use it well.");
    }

    #[test]
    fn no_frontmatter_returns_body() {
        let (frontmatter, body) = parse_frontmatter("Just text");
        assert!(frontmatter.as_object().unwrap().is_empty());
        assert_eq!(body, "Just text");
    }

    #[test]
    fn unterminated_frontmatter_returns_body() {
        let (frontmatter, body) = parse_frontmatter("---\nname: x\nno end");
        assert!(frontmatter.as_object().unwrap().is_empty());
        assert_eq!(body, "---\nname: x\nno end");
    }

    #[test]
    fn an_empty_frontmatter_block_is_no_frontmatter() {
        // TS `!yamlString`: the close follows the open, so the whole
        // document is the body (the slice bounds are inverted).
        for text in ["---\n---\nbody", "---\n---", "---\n----\nbody"] {
            let (frontmatter, body) = parse_frontmatter(text);
            assert!(frontmatter.as_object().unwrap().is_empty(), "{text}");
            assert_eq!(body, text, "{text} is the body verbatim");
        }
    }

    #[test]
    fn an_empty_first_line_between_delimiters_is_no_frontmatter() {
        // `---x\n---\nbody`: TS slice(4, 4) = "" -> falsy -> no frontmatter.
        let (frontmatter, body) = parse_frontmatter("---x\n---\nbody");
        assert!(frontmatter.as_object().unwrap().is_empty());
        assert_eq!(body, "---x\n---\nbody");
    }

    /// YAML 1.2 core scalars, as the TS `yaml` reader (and the `serde_yaml` this replaced)
    /// resolve them: the YAML 1.1 `yes`/`no`/`on`/`off` stay strings.
    #[test]
    fn scalars_resolve_by_the_yaml_1_2_core_schema() {
        let (frontmatter, _) = parse_frontmatter(
            "---\nname: x\nflag: yes\noff: no\nenabled: true\ncount: 3\ntags: [a, b]\n---\nbody",
        );
        assert_eq!(
            frontmatter,
            serde_json::json!({
                "name": "x",
                "flag": "yes",
                "off": "no",
                "enabled": true,
                "count": 3,
                "tags": ["a", "b"],
            })
        );
    }

    /// `<<` stays an ordinary key, and a duplicate key spoils the block (no frontmatter), as
    /// the YAML 1.2 readers this tracks behave.
    #[test]
    fn merge_keys_are_literal_and_duplicate_keys_are_no_frontmatter() {
        let (frontmatter, _) = parse_frontmatter("---\n<<: base\nname: x\n---\nbody");
        assert_eq!(
            frontmatter,
            serde_json::json!({ "<<": "base", "name": "x" })
        );
        let (frontmatter, body) = parse_frontmatter("---\nname: x\nname: y\n---\nbody");
        assert_eq!(frontmatter, serde_json::json!({}));
        assert_eq!(body, "body");
    }

    #[test]
    fn crlf_and_bom_normalized() {
        let (frontmatter, _) = parse_frontmatter("\u{feff}---\r\nname: x\r\n---\r\nbody");
        assert_eq!(frontmatter["name"], "x");
    }
}
