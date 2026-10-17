"""The JSON Schema subset a workflow ``agent(..., schema=...)`` result is held to.

Claude Code's Workflow tool forces a schema-carrying subagent through a
``StructuredOutput`` tool, so a mismatch is retried at the tool layer. A
kernel child answers in text instead: the workflow runtime extracts the JSON
value from the child's final answer (:func:`extract_json`), validates it here
(:func:`validate`), and asks for a corrected answer when it does not match.

Supported keywords: ``type`` (a name or a list), ``properties``,
``required``, ``additionalProperties`` (bool or schema), ``items``,
``enum``, ``const``, ``minItems``/``maxItems``, ``minLength``/``maxLength``,
``minimum``/``maximum``, ``anyOf``/``oneOf``/``allOf`` and ``description``
(ignored). Any other keyword is refused when the schema is checked, so a
schema never silently means less than it says.
"""

from __future__ import annotations

import json
import math
import re
from typing import Any

__all__ = ["SchemaError", "check_schema", "extract_json", "validate"]

_TYPES = {"object", "array", "string", "number", "integer", "boolean", "null"}
_KEYWORDS = {
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "anyOf",
    "oneOf",
    "allOf",
    "description",
    "title",
    "default",
    "examples",
    "$schema",
}
_FENCE = re.compile(r"```(?:json|JSON)?[ \t]*\n(.*?)\n[ \t]*```", re.DOTALL)


class SchemaError(ValueError):
    """A schema outside the supported subset, or one that cannot be satisfied."""


def check_schema(schema: Any) -> None:
    """Refuse a result schema the runtime cannot hold a child to.

    The root must be ``{"type": "object", "properties": {...}}`` with
    ``required`` naming only declared properties (Claude Code's rule).
    """
    if not isinstance(schema, dict):
        raise SchemaError("schema must be a dict (a JSON Schema object)")
    if schema.get("type") != "object" or not isinstance(schema.get("properties"), dict):
        raise SchemaError("schema needs {'type': 'object', 'properties': {...}} at its root")
    _check_node(schema, "$")


def _check_node(node: Any, path: str) -> None:
    if isinstance(node, bool):
        return
    if not isinstance(node, dict):
        raise SchemaError(f"{path}: a schema must be a dict or a bool")
    unknown = sorted(set(node) - _KEYWORDS)
    if unknown:
        raise SchemaError(f"{path}: unsupported schema keyword(s): {', '.join(unknown)}")
    kind = node.get("type")
    kinds = kind if isinstance(kind, list) else [kind] if kind is not None else []
    for entry in kinds:
        if entry not in _TYPES:
            raise SchemaError(f"{path}: unknown type {entry!r}")
    properties = node.get("properties")
    if properties is not None:
        if not isinstance(properties, dict):
            raise SchemaError(f"{path}.properties must be a dict")
        for name, child in properties.items():
            _check_node(child, f"{path}.properties.{name}")
    required = node.get("required")
    if required is not None:
        if not isinstance(required, list) or not all(isinstance(item, str) for item in required):
            raise SchemaError(f"{path}.required must be a list of property names")
        missing = [item for item in required if item not in (properties or {})]
        if missing:
            raise SchemaError(f"{path}.required names undeclared properties: {', '.join(missing)}")
    extra = node.get("additionalProperties")
    if extra is not None and not isinstance(extra, bool):
        _check_node(extra, f"{path}.additionalProperties")
    if "items" in node:
        _check_node(node["items"], f"{path}.items")
    for combinator in ("anyOf", "oneOf", "allOf"):
        if combinator in node:
            options = node[combinator]
            if not isinstance(options, list) or not options:
                raise SchemaError(f"{path}.{combinator} must be a non-empty list")
            for index, option in enumerate(options):
                _check_node(option, f"{path}.{combinator}[{index}]")
    if "enum" in node and (not isinstance(node["enum"], list) or not node["enum"]):
        raise SchemaError(f"{path}.enum must be a non-empty list")


def _type_matches(value: Any, kind: str) -> bool:
    if kind == "null":
        return value is None
    if kind == "boolean":
        return isinstance(value, bool)
    if kind == "integer":
        return (isinstance(value, int) and not isinstance(value, bool)) or (
            isinstance(value, float) and math.isfinite(value) and value.is_integer()
        )
    if kind == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "string":
        return isinstance(value, str)
    if kind == "array":
        return isinstance(value, list)
    return isinstance(value, dict)


def validate(value: Any, schema: Any, path: str = "$") -> list[str]:
    """Every way ``value`` misses ``schema``, as ``path: problem`` lines (empty when it matches)."""
    if schema is True:
        return []
    if schema is False:
        return [f"{path}: no value is allowed here"]
    errors: list[str] = []
    kind = schema.get("type")
    if kind is not None:
        kinds = kind if isinstance(kind, list) else [kind]
        if not any(_type_matches(value, entry) for entry in kinds):
            return [f"{path}: expected {' or '.join(kinds)}, got {_json_type(value)}"]
    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: must equal {json.dumps(schema['const'])}")
    if "enum" in schema and value not in schema["enum"]:
        allowed = ", ".join(json.dumps(item) for item in schema["enum"])
        errors.append(f"{path}: must be one of {allowed}")
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            errors.append(f"{path}: shorter than {schema['minLength']} characters")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            errors.append(f"{path}: longer than {schema['maxLength']} characters")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            errors.append(f"{path}: below the minimum {schema['minimum']}")
        if "maximum" in schema and value > schema["maximum"]:
            errors.append(f"{path}: above the maximum {schema['maximum']}")
    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            errors.append(f"{path}: fewer than {schema['minItems']} items")
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            errors.append(f"{path}: more than {schema['maxItems']} items")
        if "items" in schema:
            for index, item in enumerate(value):
                errors.extend(validate(item, schema["items"], f"{path}[{index}]"))
    if isinstance(value, dict):
        properties = schema.get("properties") or {}
        for name in schema.get("required") or []:
            if name not in value:
                errors.append(f"{path}: missing required property {name!r}")
        extra = schema.get("additionalProperties", True)
        for name, item in value.items():
            if name in properties:
                errors.extend(validate(item, properties[name], f"{path}.{name}"))
            elif extra is False:
                errors.append(f"{path}: unexpected property {name!r}")
            elif isinstance(extra, dict):
                errors.extend(validate(item, extra, f"{path}.{name}"))
    if "allOf" in schema:
        for option in schema["allOf"]:
            errors.extend(validate(value, option, path))
    if "anyOf" in schema and not any(not validate(value, option, path) for option in schema["anyOf"]):
        errors.append(f"{path}: matches none of the anyOf alternatives")
    if "oneOf" in schema:
        matched = sum(1 for option in schema["oneOf"] if not validate(value, option, path))
        if matched != 1:
            errors.append(f"{path}: matches {matched} of the oneOf alternatives, not exactly one")
    return errors


def _json_type(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


def extract_json(text: str) -> Any:
    """The JSON value a child's final answer carries.

    Tried in order: the whole answer, the last fenced ```json block, the
    last balanced ``{...}`` span. Raises ``ValueError`` when none parses.
    """
    stripped = text.strip()
    try:
        return json.loads(stripped)
    except ValueError:
        pass
    for block in reversed(_FENCE.findall(text)):
        try:
            return json.loads(block)
        except ValueError:
            continue
    end = stripped.rfind("}")
    while end != -1:
        depth = 0
        for start in range(end, -1, -1):
            char = stripped[start]
            if char == "}":
                depth += 1
            elif char == "{":
                depth -= 1
                if depth == 0:
                    try:
                        return json.loads(stripped[start : end + 1])
                    except ValueError:
                        break
        end = stripped.rfind("}", 0, end)
    raise ValueError("the answer carries no JSON value")
