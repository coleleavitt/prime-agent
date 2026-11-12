//! The authorization input a user pastes when the login's redirect does not
//! reach a localhost callback: a redirect URL, `code#state`, a
//! `code=...&state=...` query, or the bare code.

use url::Url;

/// The code and state in pasted authorization input (each `None` when absent
/// or empty).
pub(super) fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    let non_empty = |value: Option<String>| value.filter(|value| !value.is_empty());
    if let Ok(url) = Url::parse(value) {
        let get = |name: &str| {
            non_empty(
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.to_string()),
            )
        };
        return (get("code"), get("state"));
    }
    if value.contains('#') {
        let mut parts = value.splitn(2, '#');
        let code = parts.next().unwrap_or_default();
        let state = parts.next().unwrap_or_default();
        return (
            non_empty(Some(code.to_string())),
            non_empty(Some(state.to_string())),
        );
    }
    if value.contains("code=") {
        let get = |name: &str| {
            non_empty(
                url::form_urlencoded::parse(value.as_bytes())
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.to_string()),
            )
        };
        return (get("code"), get("state"));
    }
    (non_empty(Some(value.to_string())), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pasted_input_parses_like_the_ts_table() {
        assert_eq!(
            parse_authorization_input("https://auth.example/cb?code=a&state=b"),
            (Some("a".to_string()), Some("b".to_string()))
        );
        assert_eq!(
            parse_authorization_input("https://auth.example/cb?code=a%20b"),
            (Some("a b".to_string()), None)
        );
        assert_eq!(
            parse_authorization_input("the-code#the-state"),
            (Some("the-code".to_string()), Some("the-state".to_string()))
        );
        assert_eq!(
            parse_authorization_input("code=a+b&state=c"),
            (Some("a b".to_string()), Some("c".to_string()))
        );
        assert_eq!(
            parse_authorization_input(" bare "),
            (Some("bare".to_string()), None)
        );
        assert_eq!(parse_authorization_input("  "), (None, None));
        assert_eq!(
            parse_authorization_input("https://auth.example/cb?state=b"),
            (None, Some("b".to_string()))
        );
    }
}
