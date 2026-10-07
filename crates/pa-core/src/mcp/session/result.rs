//! Tool results and tool listings in the shapes `rlm.mcp` has always
//! returned: a call result is its structured content, else its joined text,
//! else its content blocks as the Python SDK's `model_dump(mode="json")`
//! dicts; a server-flagged error raises `McpToolError`.

use serde_json::{json, Map, Value};

use super::error::{McpErrorKind, McpSessionError};

/// One listed tool: `{name, description, inputSchema}` (a missing
/// description is `""`, a non-object schema `{}`).
pub(crate) fn tool_entry(tool: &rmcp::model::Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description.as_deref().unwrap_or_default(),
        "inputSchema": Value::Object(tool.input_schema.as_ref().clone()),
    })
}

/// Normalize a `tools/call` result (wire JSON) into the caller's value.
///
/// # Errors
///
/// `McpToolError` (the joined text, or a fixed message) when the server
/// flagged the result as an error.
pub(crate) fn parse_call_result(result: &Value) -> Result<Value, McpSessionError> {
    let blocks: &[Value] = result
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = if texts.is_empty() {
            "MCP tool returned an error".to_string()
        } else {
            texts.join("\n")
        };
        return Err(McpSessionError::new(McpErrorKind::Tool, message));
    }
    // Falsy-but-valid payloads ({} / []) are real results; null is absent.
    if let Some(structured) = result
        .get("structuredContent")
        .filter(|value| !value.is_null())
    {
        return Ok(structured.clone());
    }
    if !texts.is_empty() {
        return Ok(Value::String(texts.join("\n")));
    }
    if !blocks.is_empty() {
        return Ok(Value::Array(blocks.iter().map(dump_block).collect()));
    }
    Ok(json!({
        "meta": field(result, "_meta"),
        "content": [],
        "structured_content": Value::Null,
        "is_error": result.get("isError").and_then(Value::as_bool).unwrap_or(false),
        "result_type": result.get("resultType").cloned().unwrap_or_else(|| json!("complete")),
    }))
}

fn field(object: &Value, key: &str) -> Value {
    object.get(key).cloned().unwrap_or(Value::Null)
}

/// Build an ordered dict of `(python_name, wire_key)` fields.
fn dump_fields(object: &Value, fields: &[(&str, &str)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(name, key)| ((*name).to_string(), field(object, key)))
        .collect()
}

fn dump_annotations(value: &Value) -> Value {
    if value.is_object() {
        Value::Object(dump_fields(
            value,
            &[
                ("audience", "audience"),
                ("priority", "priority"),
                ("last_modified", "lastModified"),
            ],
        ))
    } else {
        Value::Null
    }
}

fn dump_icons(value: &Value) -> Value {
    match value {
        Value::Array(icons) => Value::Array(
            icons
                .iter()
                .map(|icon| {
                    Value::Object(dump_fields(
                        icon,
                        &[
                            ("src", "src"),
                            ("mime_type", "mimeType"),
                            ("sizes", "sizes"),
                            ("theme", "theme"),
                        ],
                    ))
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

fn dump_resource_contents(value: &Value) -> Value {
    let body = if value.get("text").is_some() {
        ("text", "text")
    } else {
        ("blob", "blob")
    };
    Value::Object(dump_fields(
        value,
        &[
            ("uri", "uri"),
            ("mime_type", "mimeType"),
            ("meta", "_meta"),
            body,
        ],
    ))
}

/// One non-text content block as the SDK's `model_dump(mode="json")`.
fn dump_block(block: &Value) -> Value {
    let mut dumped = match block.get("type").and_then(Value::as_str) {
        Some("image" | "audio") => dump_fields(
            block,
            &[
                ("type", "type"),
                ("data", "data"),
                ("mime_type", "mimeType"),
                ("annotations", "annotations"),
                ("meta", "_meta"),
            ],
        ),
        Some("resource_link") => {
            let mut dumped = dump_fields(
                block,
                &[
                    ("name", "name"),
                    ("title", "title"),
                    ("uri", "uri"),
                    ("description", "description"),
                    ("mime_type", "mimeType"),
                    ("size", "size"),
                    ("icons", "icons"),
                    ("annotations", "annotations"),
                    ("meta", "_meta"),
                    ("type", "type"),
                ],
            );
            dumped.insert("icons".to_string(), dump_icons(&field(block, "icons")));
            dumped
        }
        Some("resource") => {
            let mut dumped = dump_fields(
                block,
                &[
                    ("type", "type"),
                    ("resource", "resource"),
                    ("annotations", "annotations"),
                    ("meta", "_meta"),
                ],
            );
            dumped.insert(
                "resource".to_string(),
                dump_resource_contents(&field(block, "resource")),
            );
            dumped
        }
        Some("text") => dump_fields(
            block,
            &[
                ("type", "type"),
                ("text", "text"),
                ("annotations", "annotations"),
                ("meta", "_meta"),
            ],
        ),
        // A block type this client does not model passes through as sent.
        Some(_) | None => return block.clone(),
    };
    if let Some(annotations) = dumped.get_mut("annotations") {
        *annotations = dump_annotations(annotations);
    }
    Value::Object(dumped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_output_wins_and_falsy_payloads_survive() {
        for payload in [json!({}), json!([]), json!({ "issues": [1, 2] })] {
            let result = json!({ "content": [{ "type": "text", "text": "ignored" }], "structuredContent": payload });
            assert_eq!(parse_call_result(&result), Ok(payload));
        }
    }

    #[test]
    fn an_error_result_raises_with_its_text_or_a_fixed_message() {
        assert_eq!(
            parse_call_result(&json!({
                "content": [{ "type": "text", "text": "redacted failure" }],
                "isError": true
            })),
            Err(McpSessionError::new(McpErrorKind::Tool, "redacted failure"))
        );
        assert_eq!(
            parse_call_result(&json!({ "content": [], "isError": true })),
            Err(McpSessionError::new(
                McpErrorKind::Tool,
                "MCP tool returned an error"
            ))
        );
    }

    #[test]
    fn text_blocks_join_and_non_text_blocks_dump_like_the_python_sdk() {
        assert_eq!(
            parse_call_result(&json!({ "content": [
                { "type": "text", "text": "hello" },
                { "type": "image", "data": "QQ==", "mimeType": "image/png" },
                { "type": "text", "text": "world" }
            ] })),
            Ok(json!("hello\nworld"))
        );
        assert_eq!(
            parse_call_result(&json!({ "content": [
                { "type": "image", "data": "QQ==", "mimeType": "image/png",
                  "annotations": { "audience": ["user"], "priority": 0.5 }, "_meta": { "a": 1 } },
                { "type": "resource", "resource": { "uri": "file:///b", "blob": "QQ==" } },
                { "type": "resource_link", "uri": "file:///a", "name": "a",
                  "icons": [{ "src": "https://x/i.png" }] }
            ] })),
            Ok(json!([
                { "type": "image", "data": "QQ==", "mime_type": "image/png",
                  "annotations": { "audience": ["user"], "priority": 0.5, "last_modified": null },
                  "meta": { "a": 1 } },
                { "type": "resource",
                  "resource": { "uri": "file:///b", "mime_type": null, "meta": null, "blob": "QQ==" },
                  "annotations": null, "meta": null },
                { "name": "a", "title": null, "uri": "file:///a", "description": null,
                  "mime_type": null, "size": null,
                  "icons": [{ "src": "https://x/i.png", "mime_type": null, "sizes": null, "theme": null }],
                  "annotations": null, "meta": null, "type": "resource_link" }
            ]))
        );
    }

    #[test]
    fn an_empty_result_is_the_dumped_result_itself() {
        assert_eq!(
            parse_call_result(&json!({ "content": [] })),
            Ok(json!({
                "meta": null, "content": [], "structured_content": null,
                "is_error": false, "result_type": "complete"
            }))
        );
    }
}
