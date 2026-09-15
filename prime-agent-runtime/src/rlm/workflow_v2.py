"""Strict thin Python codec/client for the disabled Workflow V2 capability.

This module does not own workflow state or scheduling.  It only validates the
normative wire shapes and negotiates the native host capability.
"""
from __future__ import annotations

import json
import math
import re
from datetime import datetime
from pathlib import Path
from typing import Any

from . import repl

REQUEST_PROTOCOL = "prime.workflow.request/v2"
REPLY_PROTOCOL = "prime.workflow.result/v2"
ERROR_PROTOCOL = "prime.workflow.error/v2"
CAPABILITY_PROTOCOL = "prime.workflow.capability/v2"
_MUTATING_ACTIONS = frozenset({"create", "start", "cancel", "retry"})


class WorkflowV2WireError(ValueError):
    """A value is not a member of the closed Workflow V2 wire protocol."""


class CapabilityUnavailable(RuntimeError):
    code = "CAPABILITY_UNAVAILABLE"


_SCHEMA_PATH = Path(__file__).resolve().parents[2] / "schemas" / "workflow-v2.schema.json"
_SCHEMA: dict[str, Any] | None = None


def _schema() -> dict[str, Any]:
    global _SCHEMA
    if _SCHEMA is None:
        try:
            value = json.loads(_SCHEMA_PATH.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise CapabilityUnavailable("Workflow V2 schema is unavailable") from exc
        if not isinstance(value, dict) or not isinstance(value.get("$defs"), dict):
            raise CapabilityUnavailable("Workflow V2 schema is invalid")
        _SCHEMA = value
    return _SCHEMA


def _fail(path: str, message: str) -> None:
    raise WorkflowV2WireError(f"{path}: {message}")


def _validate(value: Any, spec: dict[str, Any], root: dict[str, Any], path: str = "$") -> None:
    if "$ref" in spec:
        prefix = "#/$defs/"
        ref = spec["$ref"]
        if not isinstance(ref, str) or not ref.startswith(prefix) or ref[len(prefix):] not in root["$defs"]:
            _fail(path, "unsupported schema reference")
        _validate(value, root["$defs"][ref[len(prefix):]], root, path)
        return
    if "const" in spec and value != spec["const"]:
        _fail(path, f"must equal {spec['const']!r}")
    if "enum" in spec and value not in spec["enum"]:
        _fail(path, "is not an allowed value")
    typ = spec.get("type")
    if isinstance(value, float) and not math.isfinite(value):
        _fail(path, "must be finite")
    matches = {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": isinstance(value, int) and not isinstance(value, bool),
        "boolean": isinstance(value, bool),
        "null": value is None,
    }
    if typ is not None and not matches.get(typ, False):
        _fail(path, f"must be {typ}")
    if isinstance(value, dict) and (typ == "object" or "properties" in spec):
        props = spec.get("properties", {})
        required = set(spec.get("required", []))
        missing = required - value.keys()
        if missing:
            _fail(path, f"missing fields {sorted(missing)}")
        if spec.get("additionalProperties") is False:
            unknown = value.keys() - props.keys()
            if unknown:
                _fail(path, f"unknown fields {sorted(unknown)}")
        for key, child in props.items():
            if key in value:
                _validate(value[key], child, root, f"{path}.{key}")
    if isinstance(value, list):
        if len(value) < spec.get("minItems", 0) or len(value) > spec.get("maxItems", len(value)):
            _fail(path, "has invalid item count")
        if spec.get("uniqueItems"):
            encoded = [json.dumps(item, sort_keys=True, separators=(",", ":"), ensure_ascii=False) for item in value]
            if len(encoded) != len(set(encoded)):
                _fail(path, "items must be unique")
        if "items" in spec:
            for index, item in enumerate(value):
                _validate(item, spec["items"], root, f"{path}[{index}]")
        if "contains" in spec and not any(_matches(item, spec["contains"], root) for item in value):
            _fail(path, "does not contain a required item")
    if isinstance(value, str):
        if len(value) < spec.get("minLength", 0) or len(value) > spec.get("maxLength", len(value)):
            _fail(path, "has invalid character length")
        if len(value.encode("utf-8")) > spec.get("x-utf8MaxBytes", len(value.encode("utf-8"))):
            _fail(path, "exceeds UTF-8 byte limit")
        if "pattern" in spec and re.search(spec["pattern"], value) is None:
            _fail(path, "does not match required pattern")
        if spec.get("format") == "date-time":
            try:
                parsed = datetime.fromisoformat(value.removesuffix("Z") + "+00:00")
            except ValueError:
                _fail(path, "must be RFC 3339 UTC date-time")
            if not value.endswith("Z") or parsed.utcoffset() is None:
                _fail(path, "must be RFC 3339 UTC date-time")
    if isinstance(value, int) and not isinstance(value, bool):
        if value < spec.get("minimum", value) or value > spec.get("maximum", value):
            _fail(path, "is outside the allowed range")
    if "oneOf" in spec and sum(_matches(value, branch, root) for branch in spec["oneOf"]) != 1:
        _fail(path, "must match exactly one closed variant")
    if "anyOf" in spec and not any(_matches(value, branch, root) for branch in spec["anyOf"]):
        _fail(path, "must match an allowed variant")
    for branch in spec.get("allOf", []):
        _validate(value, branch, root, path)
    if "not" in spec and _matches(value, spec["not"], root):
        _fail(path, "matches a forbidden variant")
    if "if" in spec:
        branch = spec.get("then") if _matches(value, spec["if"], root) else spec.get("else")
        if branch is not None:
            _validate(value, branch, root, path)


def _matches(value: Any, spec: dict[str, Any], root: dict[str, Any]) -> bool:
    try:
        _validate(value, spec, root)
        return True
    except WorkflowV2WireError:
        return False


def _validate_message_bounds(value: Any) -> None:
    count = 0
    def walk(item: Any, depth: int) -> None:
        nonlocal count
        if depth > 32:
            _fail("$", "encoded message exceeds 32 JSON levels")
        count += 1
        if count > 10_000:
            _fail("$", "encoded message exceeds 10,000 JSON nodes")
        if isinstance(item, dict):
            for key, child in item.items():
                if not isinstance(key, str):
                    _fail("$", "object keys must be strings")
                walk(child, depth + 1)
        elif isinstance(item, list):
            for child in item:
                walk(child, depth + 1)
    walk(value, 1)
    try:
        encoded = json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise WorkflowV2WireError("$: value is not strict JSON") from exc
    if len(encoded) > 1_048_576:
        _fail("$", "encoded message exceeds 1 MiB")


def _validate_definition_semantics(definition: dict[str, Any]) -> None:
    nodes = definition["nodes"]
    ids = [node["nodeId"] for node in nodes]
    if len(ids) != len(set(ids)):
        _fail("$.definition.nodes", "nodeId values must be unique")
    known = set(ids)
    if not set(definition["outputs"]) <= known:
        _fail("$.definition.outputs", "references an unknown node")
    edges: dict[str, list[str]] = {}
    for index, node in enumerate(nodes):
        dependencies = [dependency["nodeId"] for dependency in node["dependsOn"]]
        if len(dependencies) != len(set(dependencies)):
            _fail(f"$.definition.nodes[{index}].dependsOn", "dependency nodeId values must be unique")
        if node["nodeId"] in dependencies or not set(dependencies) <= known:
            _fail(f"$.definition.nodes[{index}].dependsOn", "contains a self or unknown dependency")
        edges[node["nodeId"]] = dependencies
        if definition["budget"]["maxTotalTokens"] < node["maxTokens"]:
            _fail("$.definition.budget.maxTotalTokens", f"must be at least maxTokens for node {node['nodeId']!r}")
    visiting: set[str] = set()
    visited: set[str] = set()
    def visit(node_id: str) -> None:
        if node_id in visiting:
            _fail("$.definition.nodes", "dependency graph must be acyclic")
        if node_id not in visited:
            visiting.add(node_id)
            for dependency in edges[node_id]:
                visit(dependency)
            visiting.remove(node_id)
            visited.add(node_id)
    for node_id in ids:
        visit(node_id)


def _definition(name: str) -> dict[str, Any]:
    root = _schema()
    try:
        return root["$defs"][name]
    except KeyError as exc:
        raise CapabilityUnavailable(f"Workflow V2 schema lacks {name}") from exc


def _validate_request(request: Any) -> dict[str, Any]:
    """Validate one closed public controller request."""
    root = _schema()
    _validate(request, {"oneOf": [{"$ref": f"#/$defs/{name}"} for name in (
        "validateRequest", "createRequest", "startRequest", "cancelRequest",
        "retryRequest", "statusRequest", "eventsRequest")]} , root)
    _validate_message_bounds(request)
    if "definition" in request:
        _validate_definition_semantics(request["definition"])
    return dict(request)


def _validate_reply(reply: Any, *, request_id: str | None = None, action: str | None = None) -> dict[str, Any]:
    """Validate one closed public controller result or error."""
    root = _schema()
    _validate_message_bounds(reply)
    names = ("validateResult", "createResult", "commandResult", "statusResult", "eventsResult", "publicError")
    _validate(reply, {"oneOf": [{"$ref": f"#/$defs/{name}"} for name in names]}, root)
    if request_id is not None and reply.get("requestId") != request_id:
        _fail("$.requestId", "does not correlate with request")
    if action is not None and reply.get("protocol") != ERROR_PROTOCOL and reply.get("action") != action:
        _fail("$.action", "does not correlate with request")
    return dict(reply)


def _validate_capability(value: Any) -> dict[str, Any]:
    root = _schema()
    _validate(value, _definition("capability"), root)
    return dict(value)



async def request(*, request: dict[str, Any]) -> dict[str, Any]:
    """Call the native V2 service through its sole closed request envelope."""
    checked = _validate_request(request)
    if checked["action"] in _MUTATING_ACTIONS:
        raise CapabilityUnavailable(f"Workflow V2 {checked['action']} is unavailable in this release slice")
    envelope = {"type": "workflow.v2.request", "request": checked}
    try:
        raw = await repl.host_request(envelope)
    except (repl.HostRequestUnavailable, repl.HostConnectionLost, repl.HostDrainTimeout) as exc:
        raise CapabilityUnavailable(str(exc) or "Workflow V2 capability is unavailable") from exc
    if not isinstance(raw, dict) or raw.get("status") != "ok" or set(raw) != {"status", "result"}:
        message = raw.get("error", "Workflow V2 capability is unavailable") if isinstance(raw, dict) else "Workflow V2 capability is unavailable"
        raise CapabilityUnavailable(str(message))
    return _validate_reply(raw["result"], request_id=checked["requestId"], action=checked["action"])
