//! Failure fingerprints: the normalized identity of a runtime failure, shared
//! by the failure ledger, the resolution index, and the `failure.fingerprint`
//! span attribute (TS `failure-ledger.ts`).
//!
//! One rule set produces every id, so a span key, a ledger key and a
//! resolution key for the same failure are the same string, in this binary
//! and in the TS one.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::js::{
    collapse_js_whitespace, js_len, js_prefix, js_trim, json_string, JS_WHITESPACE_CLASS,
};

/// Criterion ids of failure opponents in a RAVO gate: `failure:<id>`.
pub const FAILURE_OPPONENT_PREFIX: &str = "failure:";

/// The span attribute that carries a failed tool call's fingerprint id.
pub const FAILURE_FINGERPRINT_ATTR: &str = "failure.fingerprint";

const MAX_NORMALIZED_MESSAGE_LENGTH: usize = 200;
/// Longest excerpt kept for an observation (UTF-16 units, before `...`).
pub(crate) const MAX_EXCERPT_LENGTH: usize = 400;
/// Longest quoted span the normalizer folds to `?` (UTF-16 units inside the quotes).
const MAX_QUOTED_LENGTH: usize = 400;
const TRACEBACK_HEADER: &str = "Traceback (most recent call last)";
const TOOL_ERROR_WITHOUT_OUTPUT: &str = "tool returned an error without output";

/// What kind of failure was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// A Python traceback in a tool's output.
    PythonException,
    /// A tool result flagged as an error, without a traceback.
    ToolError,
    /// An assistant message that ended with `stopReason: "error"`.
    ProviderError,
}

impl FailureKind {
    /// The wire name (`python_exception`, `tool_error`, `provider_error`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PythonException => "python_exception",
            Self::ToolError => "tool_error",
            Self::ProviderError => "provider_error",
        }
    }

    /// Parse a wire name.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "python_exception" => Some(Self::PythonException),
            "tool_error" => Some(Self::ToolError),
            "provider_error" => Some(Self::ProviderError),
            _ => None,
        }
    }
}

/// A failure's normalized identity. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureFingerprint {
    /// First 16 hex chars of `sha256(canonicalJson({kind, source, exceptionClass, message}))`.
    pub id: String,
    pub kind: FailureKind,
    /// Normalized message: lowercase, numbers `#`, quoted strings `?`, paths
    /// `<path>`, hex ids `<hex>`, whitespace collapsed, at most 200 UTF-16 units.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<String>,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("invalid built-in pattern {pattern}: {error}"))
}

static PATH: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?:~|[a-z]:)?(?:/[^{JS_WHITESPACE_CLASS}'"`,;()\[\]<>]+)+"#
    ))
});
static HEX_LITERAL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?-u:\b)0x[0-9a-f]+(?-u:\b)"));
static HEX_RUN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?-u:\b)[0-9a-f]{8,}(?:-[0-9a-f]{4,}){0,4}(?-u:\b)"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"[-+]?[0-9]+(?:\.[0-9]+)?"));
static TRACEBACK_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"^[{JS_WHITESPACE_CLASS}]*File "([^"]+)", line [0-9]+"#
    ))
});
static EXCEPTION_LINE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^([A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*(?:Error|Exception|Warning)): (.*)$")
});
static BARE_EXCEPTION_LINE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^([A-Z][A-Za-z0-9_]*(?:\.[A-Z][A-Za-z0-9_]*)*)$"));

/// An ECMAScript line terminator (what `.` never matches and a multiline
/// `^`/`$` matches beside).
pub(crate) fn is_js_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The TS quoted-string fold: a quote (double, single, or backtick), up to
/// 400 UTF-16 units on one line without that quote, and the same quote
/// again become `?` (`/(["'X])(?:(?!\1).){0,400}\1/g` with X the backtick).
fn fold_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while let Some(open) = rest.chars().next() {
        if matches!(open, '"' | '\'' | '`') {
            let body = &rest[open.len_utf8()..];
            let mut units = 0;
            for (offset, ch) in body.char_indices() {
                if ch == open {
                    out.push('?');
                    rest = &body[offset + ch.len_utf8()..];
                    continue 'scan;
                }
                units += ch.len_utf16();
                if is_js_line_terminator(ch) || units > MAX_QUOTED_LENGTH {
                    break;
                }
            }
        }
        out.push(open);
        rest = &rest[open.len_utf8()..];
    }
    out
}

/// Canonicalize a raw error message so the same fault reported with
/// different numbers, paths, ids, or quoted values maps to one fingerprint.
#[must_use]
pub fn normalize_failure_message(raw: &str) -> String {
    let text = raw.to_lowercase();
    let text = PATH.replace_all(&text, "<path>");
    let text = fold_quoted(&text);
    let text = HEX_LITERAL.replace_all(&text, "<hex>");
    let text = HEX_RUN.replace_all(&text, "<hex>");
    let text = NUMBER.replace_all(&text, "#");
    let text = collapse_js_whitespace(&text);
    let text = js_trim(&text);
    if js_len(text) > MAX_NORMALIZED_MESSAGE_LENGTH {
        js_prefix(text, MAX_NORMALIZED_MESSAGE_LENGTH).to_string()
    } else {
        text.to_string()
    }
}

/// `canonicalJson(...)` of the hashed identity: keys sorted, absent
/// optionals as `null`.
fn canonical_identity(
    kind: FailureKind,
    source: Option<&str>,
    exception_class: Option<&str>,
    message: &str,
) -> String {
    let nullable = |value: Option<&str>| value.map_or_else(|| "null".to_string(), json_string);
    format!(
        "{{\"exceptionClass\":{},\"kind\":{},\"message\":{},\"source\":{}}}",
        nullable(exception_class),
        json_string(kind.as_str()),
        json_string(message),
        nullable(source),
    )
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Fingerprint one failure.
#[must_use]
pub fn fingerprint_failure(
    kind: FailureKind,
    source: Option<&str>,
    exception_class: Option<&str>,
    raw_message: &str,
) -> FailureFingerprint {
    let message = normalize_failure_message(raw_message);
    let digest = Sha256::digest(canonical_identity(kind, source, exception_class, &message));
    FailureFingerprint {
        id: hex(&digest[..8]),
        kind,
        message,
        source: source.map(str::to_string),
        exception_class: exception_class.map(str::to_string),
    }
}

/// The opponent criterion id of a fingerprint id (idempotent).
#[must_use]
pub fn failure_opponent_id(id: &str) -> String {
    if id.starts_with(FAILURE_OPPONENT_PREFIX) {
        id.to_string()
    } else {
        format!("{FAILURE_OPPONENT_PREFIX}{id}")
    }
}

/// `text.trim()`, clipped to 400 UTF-16 units with a `...` marker.
#[must_use]
pub(crate) fn clip_excerpt(text: &str) -> String {
    let trimmed = js_trim(text);
    if js_len(trimmed) > MAX_EXCERPT_LENGTH {
        format!("{}...", js_prefix(trimmed, MAX_EXCERPT_LENGTH))
    } else {
        trimmed.to_string()
    }
}

/// The last Python traceback in a block of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTraceback {
    pub exception_class: String,
    pub message: String,
    /// The last frame line and the exception line, clipped.
    pub excerpt: String,
    /// The skill a `.../skills/<name>/...` frame belongs to.
    pub skill_name: Option<String>,
}

/// `text.split(/\r?\n/)`.
fn split_js_lines(text: &str) -> Vec<&str> {
    let mut pieces: Vec<&str> = text.split('\n').collect();
    let last = pieces.len() - 1;
    for piece in &mut pieces[..last] {
        if let Some(stripped) = piece.strip_suffix('\r') {
            *piece = stripped;
        }
    }
    pieces
}

/// `EXCEPTION_LINE.exec(line)` under the `m` flag: the first terminator-free
/// stretch of the line that matches as a whole.
fn exception_line(line: &str) -> Option<(String, String)> {
    line.split(is_js_line_terminator).find_map(|segment| {
        EXCEPTION_LINE
            .captures(segment)
            .map(|captures| (captures[1].to_string(), captures[2].to_string()))
    })
}

/// Parse the LAST Python traceback in a block of tool output. The exception
/// class comes from the final `Class: message` line after the header; a bare
/// class line (e.g. `KeyboardInterrupt`) after a frame is the fallback.
#[must_use]
pub fn parse_python_traceback(text: &str) -> Option<ParsedTraceback> {
    let last_header = text.rfind(TRACEBACK_HEADER)?;
    let block = &text[last_header..];
    let mut exception_class: Option<String> = None;
    let mut message = String::new();
    let mut last_frame: Option<String> = None;
    let mut skill_name: Option<String> = None;
    for line in split_js_lines(block) {
        if let Some(frame) = TRACEBACK_FRAME.captures(line) {
            last_frame = Some(js_trim(line).to_string());
            if let Some(skill) = skill_name_from_path(&frame[1]) {
                skill_name = Some(skill);
            }
            continue;
        }
        if let Some((class, text)) = exception_line(line) {
            exception_class = Some(class);
            message = js_trim(&text).to_string();
            continue;
        }
        if let Some(bare) = BARE_EXCEPTION_LINE.captures(js_trim(line)) {
            if last_frame.is_some() && exception_class.is_none() {
                exception_class = Some(bare[1].to_string());
                message.clear();
            }
        }
    }
    let exception_class = exception_class?;
    let exception_line = if message.is_empty() {
        exception_class.clone()
    } else {
        format!("{exception_class}: {message}")
    };
    let excerpt = clip_excerpt(&match &last_frame {
        Some(frame) => format!("{frame}\n{exception_line}"),
        None => exception_line,
    });
    Some(ParsedTraceback {
        exception_class,
        message,
        excerpt,
        skill_name,
    })
}

/// The skill directory a traceback frame's path runs through
/// (`.../skills/<name>/...`), when it names one.
fn skill_name_from_path(path: &str) -> Option<String> {
    // `path.split(/[\\/]+/)`: runs of separators split once.
    let raw: Vec<&str> = path.split(['/', '\\']).collect();
    let last = raw.len() - 1;
    let parts: Vec<&str> = raw
        .iter()
        .enumerate()
        .filter(|(index, part)| !part.is_empty() || *index == 0 || *index == last)
        .map(|(_, part)| *part)
        .collect();
    let index = parts.iter().rposition(|part| *part == "skills")?;
    let name = parts.get(index + 1)?;
    (!name.is_empty() && !name.contains('.')).then(|| (*name).to_string())
}

/// The fingerprint a tool result carries, or `None` when it is not a
/// failure: a traceback anywhere in the text wins over the error flag.
///
/// The span attribute, the ledger and the resolution index all key on
/// this, so the three can never drift apart.
#[must_use]
pub fn fingerprint_tool_result_text(
    tool_name: Option<&str>,
    text: &str,
    is_error: bool,
) -> Option<FailureFingerprint> {
    if let Some(traceback) = parse_python_traceback(text) {
        return Some(fingerprint_failure(
            FailureKind::PythonException,
            traceback.skill_name.as_deref().or(tool_name),
            Some(&traceback.exception_class),
            &traceback.message,
        ));
    }
    is_error.then(|| {
        fingerprint_failure(
            FailureKind::ToolError,
            tool_name,
            None,
            tool_error_text(text),
        )
    })
}

/// The raw message of a flagged tool error: its trimmed text, or a
/// placeholder when it printed nothing.
pub(crate) fn tool_error_text(text: &str) -> &str {
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        TOOL_ERROR_WITHOUT_OUTPUT
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_numbers_quotes_paths_hex_and_whitespace() {
        assert_eq!(
            normalize_failure_message(
                "  [Errno 2] No such file: \"/var/tmp/run-42/out.json\"   at   0x7ffe1234 id=3f2a9c8e7b6d5a4f   line 17\n"
            ),
            "[errno #] no such file: ? at <hex> id=<hex> line #"
        );
        assert_eq!(js_len(&normalize_failure_message(&"x".repeat(500))), 200);
    }

    #[test]
    fn a_quote_closes_only_on_its_own_line_within_400_units() {
        assert_eq!(fold_quoted("a 'b' c"), "a ? c");
        assert_eq!(fold_quoted("a 'b\nc' d"), "a 'b\nc' d");
        assert_eq!(fold_quoted(&format!("'{}'", "x".repeat(400))), "?");
        let long = format!("'{}'", "x".repeat(401));
        assert_eq!(fold_quoted(&long), long);
        assert_eq!(fold_quoted("\"a'b\" 'c"), "? 'c");
    }

    #[test]
    fn same_error_with_different_numbers_and_paths_shares_an_id() {
        let a = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("FileNotFoundError"),
            "[Errno 2] No such file or directory: '/tmp/build-1/out.txt'",
        );
        let b = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("FileNotFoundError"),
            "[Errno 2] No such file or directory: '/home/other/build-77/result.txt'",
        );
        assert_eq!(a, b);
        assert_eq!(a.id.len(), 16);
        assert!(a.id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn kind_source_and_class_each_change_the_id() {
        let base = fingerprint_failure(FailureKind::ToolError, Some("bash"), None, "exit 1");
        for other in [
            fingerprint_failure(FailureKind::ToolError, Some("ipython"), None, "exit 1"),
            fingerprint_failure(FailureKind::PythonException, Some("bash"), None, "exit 1"),
            fingerprint_failure(
                FailureKind::ToolError,
                Some("bash"),
                Some("RuntimeError"),
                "exit 1",
            ),
        ] {
            assert_ne!(other.id, base.id);
        }
        assert_eq!(
            base.id,
            fingerprint_failure(FailureKind::ToolError, Some("bash"), None, "exit 001").id
        );
        assert_eq!(
            failure_opponent_id(&base.id),
            format!("failure:{}", base.id)
        );
        assert_eq!(
            failure_opponent_id(&failure_opponent_id(&base.id)),
            format!("failure:{}", base.id)
        );
    }

    #[test]
    fn a_bare_exception_class_line_is_the_fallback() {
        let parsed = parse_python_traceback(
            "Traceback (most recent call last):\n  File \"/tmp/x.py\", line 1, in <module>\nKeyboardInterrupt\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            ParsedTraceback {
                exception_class: "KeyboardInterrupt".to_string(),
                message: String::new(),
                excerpt: "File \"/tmp/x.py\", line 1, in <module>\nKeyboardInterrupt".to_string(),
                skill_name: None,
            }
        );
    }

    #[test]
    fn the_skill_frame_names_the_source() {
        assert_eq!(
            skill_name_from_path("/home/u/.agents/skills/deploy-widget/scripts/run.py"),
            Some("deploy-widget".to_string())
        );
        assert_eq!(
            skill_name_from_path("C:\\x\\skills\\\\w\\a.py"),
            Some("w".to_string())
        );
        assert_eq!(skill_name_from_path("/x/skills/a.py"), None);
        assert_eq!(skill_name_from_path("/x/skills/"), None);
    }

    #[test]
    fn the_tool_result_fingerprint_agrees_with_the_ledger_rules() {
        let traceback = "Traceback (most recent call last):\n  File \"<cell>\", line 1, in <module>\n    websearch(query)\nKeyError: 'results'";
        let via_hook = fingerprint_tool_result_text(Some("ipython"), traceback, false).unwrap();
        let via_ledger = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            Some("KeyError"),
            "'results'",
        );
        assert_eq!(via_hook, via_ledger);
        assert_eq!(
            fingerprint_tool_result_text(Some("edit"), "file not found", true),
            Some(fingerprint_failure(
                FailureKind::ToolError,
                Some("edit"),
                None,
                "file not found"
            ))
        );
        assert_eq!(
            fingerprint_tool_result_text(Some("ipython"), "42\n", false),
            None
        );
    }
}
