//! Header helpers.

/// Default a JSON request's `Content-Type` to `application/json` (#3351): the TS SDKs always send
/// it, `reqwest` sets none for a string body, and strict servers answer 415 without it. A content
/// type the caller already configured (model or option headers) is kept, so no duplicate is sent.
pub fn ensure_json_content_type(headers: &mut Vec<(String, String)>) {
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
}
