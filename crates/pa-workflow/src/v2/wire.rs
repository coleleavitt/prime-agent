//! The Workflow V2 wire (TS `workflow-v2-wire.ts`, runtime
//! `rlm/workflow_v2.py`): strict closed decoders for every protocol in the
//! V2 family, the one definition semantic validator, RFC 8785 canonical
//! JSON and its digests, and the public reply builders.
//!
//! Every decoder validates against the embedded schema ([`super::schema`])
//! and then checks result byte/digest bindings; definition-bearing entry
//! points add the graph and budget semantics (`WORKFLOW-V2.md` §4).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{json, schema};

/// The public request protocol.
pub const REQUEST_PROTOCOL: &str = "prime.workflow.request/v2";
/// The public success-reply protocol.
pub const REPLY_PROTOCOL: &str = "prime.workflow.result/v2";
/// The public error-reply protocol.
pub const ERROR_PROTOCOL: &str = "prime.workflow.error/v2";
/// The definition protocol.
pub const DEFINITION_PROTOCOL: &str = "prime.workflow.definition/v2";
/// The schema's `boundedText` ceiling (UTF-8 bytes and code points).
pub const MAX_BOUNDED_TEXT: usize = 512;

/// A value outside the closed V2 wire, with its JSON path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path} {reason}")]
pub struct WireError {
    path: String,
    reason: String,
}

impl WireError {
    pub(crate) fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        WireError {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

/// One public controller action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Validate,
    Create,
    Start,
    Cancel,
    Retry,
    Status,
    Events,
}

impl Action {
    /// Every action, in the schema's order.
    pub const ALL: [Action; 7] = [
        Action::Validate,
        Action::Create,
        Action::Start,
        Action::Cancel,
        Action::Retry,
        Action::Status,
        Action::Events,
    ];

    /// The wire name.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Action::Validate => "validate",
            Action::Create => "create",
            Action::Start => "start",
            Action::Cancel => "cancel",
            Action::Retry => "retry",
            Action::Status => "status",
            Action::Events => "events",
        }
    }

    /// The action a wire name names.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Action> {
        Action::ALL
            .into_iter()
            .find(|action| action.wire_name() == name)
    }

    fn request_def(self) -> &'static str {
        match self {
            Action::Validate => "validateRequest",
            Action::Create => "createRequest",
            Action::Start => "startRequest",
            Action::Cancel => "cancelRequest",
            Action::Retry => "retryRequest",
            Action::Status => "statusRequest",
            Action::Events => "eventsRequest",
        }
    }

    fn result_def(self) -> &'static str {
        match self {
            Action::Validate => "validateResult",
            Action::Create => "createResult",
            Action::Start | Action::Cancel | Action::Retry => "commandResult",
            Action::Status => "statusResult",
            Action::Events => "eventsResult",
        }
    }
}

/// One agent node of a definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDefinition {
    pub node_id: String,
    pub prompt: String,
    pub depends_on: Vec<Dependency>,
    pub model: String,
    pub max_tokens: u64,
}

/// One dependency edge (`require` is always `accepted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub node_id: String,
}

/// The soft admission budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub max_concurrent_attempts: u64,
    pub max_total_tokens: u64,
}

/// A decoded, semantically valid `prime.workflow.definition/v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub nodes: Vec<NodeDefinition>,
    pub outputs: Vec<String>,
    pub budget: Budget,
    /// `sha256:` of the definition's RFC 8785 bytes.
    pub digest: String,
}

/// The typed view of a schema-valid definition (the constant fields are
/// the schema's; serde ignores them).
#[derive(Deserialize)]
struct DefinitionFields {
    nodes: Vec<NodeDefinition>,
    outputs: Vec<String>,
    budget: Budget,
}

/// A decoded public request: its correlation and its action. The value
/// itself stays available for the canonical request digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicRequest {
    pub request_id: String,
    pub action: Action,
    /// The validated definition (`validate`, `create`).
    pub definition: Option<Definition>,
    /// The run selector (every post-create action).
    pub run_id: Option<String>,
}

pub(crate) fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// RFC 8785 canonical JSON of a V2 value: object keys in UTF-16 code-unit
/// order, no insignificant whitespace, ECMAScript string escaping. Every V2
/// number is an integer, so a fractional number is refused rather than
/// guessed at.
///
/// # Errors
///
/// A non-integer number.
pub fn canonical_json(value: &Value) -> Result<String, WireError> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), WireError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                let _ = write!(out, "{integer}");
            } else if let Some(integer) = number.as_u64() {
                let _ = write!(out, "{integer}");
            } else {
                return Err(WireError::new("$", "has a non-integer canonical number"));
            }
        }
        // serde_json escapes exactly as `JSON.stringify` does for valid
        // Unicode: `"`, `\`, and C0 controls (short forms, else `\u00xx`).
        Value::String(text) => out.push_str(&Value::String(text.clone()).to_string()),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&object[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// `sha256:` of a value's canonical bytes: the request digest of a public
/// mutation (§6) and the definition digest.
///
/// # Errors
///
/// A non-integer number.
pub fn request_digest(value: &Value) -> Result<String, WireError> {
    Ok(sha256_digest(canonical_json(value)?.as_bytes()))
}

/// Validate against one `$defs` entry, then the result byte/digest
/// bindings.
fn decode(value: &Value, def: &str) -> Result<(), WireError> {
    schema::validate_def(value, def)?;
    validate_digest_bindings(value, "$")
}

/// Every object carrying `text`, `utf8Bytes`, and `sha256` must bind them:
/// the byte count and digest are the text's.
fn validate_digest_bindings(value: &Value, path: &str) -> Result<(), WireError> {
    match value {
        Value::Object(object) => {
            if let (Some(Value::String(text)), Some(bytes), Some(digest)) = (
                object.get("text"),
                object.get("utf8Bytes"),
                object.get("sha256"),
            ) {
                let observed = u64::try_from(text.len()).unwrap_or(u64::MAX);
                if bytes.as_u64() != Some(observed)
                    || digest.as_str() != Some(sha256_digest(text.as_bytes()).as_str())
                {
                    return Err(WireError::new(path, "has a result byte/digest mismatch"));
                }
            }
            for (key, child) in object {
                validate_digest_bindings(child, &format!("{path}.{key}"))?;
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                validate_digest_bindings(child, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The one definition semantic validator (§4): unique node ids, known
/// outputs and dependencies, no duplicate or self edge, acyclic, and a
/// total budget that covers every node's cap.
fn validate_definition_semantics(
    definition: &DefinitionFields,
    path: &str,
) -> Result<(), WireError> {
    let mut known = HashSet::new();
    for node in &definition.nodes {
        if !known.insert(node.node_id.as_str()) {
            return Err(WireError::new(
                format!("{path}.nodes"),
                "nodeId values must be unique",
            ));
        }
    }
    if let Some(unknown) = definition
        .outputs
        .iter()
        .find(|output| !known.contains(output.as_str()))
    {
        return Err(WireError::new(
            format!("{path}.outputs"),
            format!("references unknown node {}", Value::String(unknown.clone())),
        ));
    }
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for (index, node) in definition.nodes.iter().enumerate() {
        let at = format!("{path}.nodes[{index}].dependsOn");
        let dependencies: Vec<&str> = node
            .depends_on
            .iter()
            .map(|dependency| dependency.node_id.as_str())
            .collect();
        if dependencies.iter().collect::<HashSet<_>>().len() != dependencies.len() {
            return Err(WireError::new(
                at,
                "dependency nodeId values must be unique",
            ));
        }
        for dependency in &dependencies {
            if *dependency == node.node_id {
                return Err(WireError::new(at, "contains a self dependency"));
            }
            if !known.contains(dependency) {
                return Err(WireError::new(
                    at,
                    format!(
                        "references unknown node {}",
                        Value::String((*dependency).to_string())
                    ),
                ));
            }
        }
        edges.insert(node.node_id.as_str(), dependencies);
        if definition.budget.max_total_tokens < node.max_tokens {
            return Err(WireError::new(
                format!("{path}.budget.maxTotalTokens"),
                format!(
                    "must be at least maxTokens for node {}",
                    Value::String(node.node_id.clone())
                ),
            ));
        }
    }
    // Iterative three-colour DFS: a back edge is a cycle.
    let mut done: HashSet<&str> = HashSet::new();
    for node in &definition.nodes {
        let root = node.node_id.as_str();
        if done.contains(root) {
            continue;
        }
        let mut on_path: HashSet<&str> = HashSet::from([root]);
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        while let Some((current, next)) = stack.last_mut() {
            let current = *current;
            if let Some(&dependency) = edges[current].get(*next) {
                *next += 1;
                if on_path.contains(dependency) {
                    return Err(WireError::new(
                        format!("{path}.nodes"),
                        "dependency graph must be acyclic",
                    ));
                }
                if !done.contains(dependency) {
                    on_path.insert(dependency);
                    stack.push((dependency, 0));
                }
            } else {
                stack.pop();
                on_path.remove(current);
                done.insert(current);
            }
        }
    }
    Ok(())
}

fn decode_definition_at(value: &Value, path: &str) -> Result<Definition, WireError> {
    let fields: DefinitionFields = serde_json::from_value(value.clone())
        .map_err(|_| WireError::new(path, "does not decode as a definition"))?;
    validate_definition_semantics(&fields, path)?;
    Ok(Definition {
        nodes: fields.nodes,
        outputs: fields.outputs,
        budget: fields.budget,
        digest: request_digest(value)?,
    })
}

/// Decode one `prime.workflow.definition/v2`.
///
/// # Errors
///
/// The first structural or semantic violation.
pub fn decode_definition(value: &Value) -> Result<Definition, WireError> {
    decode(value, "definition")?;
    decode_definition_at(value, "$")
}

/// Why a public request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The envelope is outside the closed request family.
    #[error(transparent)]
    Request(WireError),
    /// The envelope is a closed `validate`/`create`, but its definition is
    /// structurally or semantically invalid.
    #[error(transparent)]
    Definition(WireError),
}

/// Decode one public request: the variant its `action` names (an unknown
/// action is checked as `validate`, which then fails), the message bounds,
/// and, for `validate` and `create`, the definition's structure and
/// semantics — reported apart from envelope violations.
///
/// # Errors
///
/// The first violation.
pub fn decode_public_request(value: &Value) -> Result<PublicRequest, RequestError> {
    json::check_bounds(value).map_err(RequestError::Request)?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .and_then(Action::from_wire)
        .unwrap_or(Action::Validate);
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let definition = match action {
        Action::Validate | Action::Create => {
            // The definition-bearing envelopes are exactly these four keys;
            // the definition is then checked on its own so its failures
            // stay distinguishable.
            check_definition_envelope(value, action).map_err(RequestError::Request)?;
            let definition = &value["definition"];
            let definition = schema::validate_def_at(definition, "definition", "$.definition")
                .and_then(|()| decode_definition_at(definition, "$.definition"))
                .map_err(RequestError::Definition)?;
            // The whole request against its schema variant (and the digest
            // bindings): by construction the same verdict, kept as the one
            // authority.
            decode(value, action.request_def()).map_err(RequestError::Request)?;
            Some(definition)
        }
        _ => {
            decode(value, action.request_def()).map_err(RequestError::Request)?;
            None
        }
    };
    Ok(PublicRequest {
        request_id: text("requestId").unwrap_or_default(),
        action,
        definition,
        run_id: text("runId"),
    })
}

fn check_definition_envelope(value: &Value, action: Action) -> Result<(), WireError> {
    let Value::Object(object) = value else {
        return Err(WireError::new("$", "must be an object"));
    };
    for key in ["protocol", "requestId", "action", "definition"] {
        if !object.contains_key(key) {
            return Err(WireError::new(format!("$.{key}"), "is required"));
        }
    }
    if let Some(unknown) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "protocol" | "requestId" | "action" | "definition"
        )
    }) {
        return Err(WireError::new(format!("$.{unknown}"), "is unknown"));
    }
    if object["protocol"] != REQUEST_PROTOCOL {
        return Err(WireError::new("$.protocol", "has the wrong constant"));
    }
    if !object["requestId"].as_str().is_some_and(schema::is_id) {
        return Err(WireError::new("$.requestId", "has invalid syntax"));
    }
    if object["action"] != action.wire_name() {
        return Err(WireError::new("$.action", "has the wrong constant"));
    }
    Ok(())
}

/// Decode one public success reply (the variant its `action` names).
///
/// # Errors
///
/// The first violation.
pub fn decode_public_result(value: &Value) -> Result<(), WireError> {
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .and_then(Action::from_wire)
        .unwrap_or(Action::Validate);
    decode(value, action.result_def())
}

/// Decode one value of a single-variant `$defs` entry: `publicError`,
/// `controllerEvent`, `retainedResult`, `retainedError`, `retainedEvent`,
/// `capability`, `view`, or `turnSettlement`.
///
/// # Errors
///
/// The first violation.
pub fn decode_as(value: &Value, def: Def) -> Result<(), WireError> {
    decode(value, def.name())
}

/// The single-variant `$defs` entries [`decode_as`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Def {
    PublicError,
    ControllerEvent,
    RetainedResult,
    RetainedError,
    RetainedEvent,
    Capability,
    View,
    TurnSettlement,
}

impl Def {
    fn name(self) -> &'static str {
        match self {
            Def::PublicError => "publicError",
            Def::ControllerEvent => "controllerEvent",
            Def::RetainedResult => "retainedResult",
            Def::RetainedError => "retainedError",
            Def::RetainedEvent => "retainedEvent",
            Def::Capability => "capability",
            Def::View => "view",
            Def::TurnSettlement => "turnSettlement",
        }
    }
}

/// Decode one controller-to-host retained request (the variant its
/// `operation` names; an unknown operation is checked as `child.get`).
///
/// # Errors
///
/// The first violation.
pub fn decode_retained_request(value: &Value) -> Result<(), WireError> {
    let def = match value.get("operation").and_then(Value::as_str) {
        Some("child.admit") => "childAdmitRequest",
        Some("child.send") => "childSendRequest",
        Some("child.list") => "childListRequest",
        Some("child.events") => "childEventsRequest",
        Some("child.wait") => "childWaitRequest",
        Some("child.cancel") => "childCancelRequest",
        Some("child.delete") => "childDeleteRequest",
        _ => "childGetRequest",
    };
    decode(value, def)
}

/// Strictly parse and decode one public request from wire bytes.
///
/// # Errors
///
/// The first codec or contract violation.
pub fn decode_public_request_json(input: &[u8]) -> Result<PublicRequest, RequestError> {
    decode_public_request(&json::parse(input).map_err(RequestError::Request)?)
}

/// A closed public error code (`publicError.code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidDefinition,
    CapabilityUnavailable,
}

/// Cut `text` to the schema's `boundedText` (512 code points, 512 bytes)
/// on a character boundary.
#[must_use]
pub fn bounded_text(text: &str) -> String {
    let mut end = 0;
    for (count, (index, ch)) in text.char_indices().enumerate() {
        if count == MAX_BOUNDED_TEXT || index + ch.len_utf8() > MAX_BOUNDED_TEXT {
            break;
        }
        end = index + ch.len_utf8();
    }
    text[..end].to_string()
}

/// The closed `validateResult` for a valid definition (`digest` set) or an
/// invalid one (`errors` set).
#[must_use]
pub fn validate_result(request_id: &str, outcome: Result<&Definition, &WireError>) -> Value {
    let (valid, digest, errors) = match outcome {
        Ok(definition) => (true, Value::String(definition.digest.clone()), Vec::new()),
        Err(error) => (
            false,
            Value::Null,
            vec![Value::String(bounded_text(&error.to_string()))],
        ),
    };
    serde_json::json!({
        "protocol": REPLY_PROTOCOL,
        "requestId": request_id,
        "action": Action::Validate.wire_name(),
        "valid": valid,
        "definitionDigest": digest,
        "errors": errors,
        "warnings": [],
    })
}

/// The closed `publicError` reply (no revision: nothing was read).
#[must_use]
pub fn public_error(request_id: &str, code: ErrorCode, message: &str) -> Value {
    serde_json::json!({
        "protocol": ERROR_PROTOCOL,
        "requestId": request_id,
        "code": code,
        "message": bounded_text(message),
        "retryable": false,
        "currentRevision": null,
    })
}

#[cfg(test)]
mod tests;
