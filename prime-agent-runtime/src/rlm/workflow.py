"""Closed kernel adapter for Workflow V1's sole native operation."""
from __future__ import annotations

import hashlib
import math
import re
from typing import Any

from . import repl

REQUEST_PROTOCOL = "prime.workflow.run-agent/v1"
REPLY_PROTOCOL = "prime.workflow.run-agent-result/v1"
_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
_MODEL = re.compile(r"^[^\s]+/[^\s]+$")
_SAFE_INT = 9_007_199_254_740_991
_FAILURES = {
    "model_resolution_failed": "MODEL_RESOLUTION_FAILED",
    "provider_failed": "PROVIDER_FAILED",
    "result_missing": "RESULT_MISSING",
    "result_too_large": "RESULT_TOO_LARGE",
    "usage_invalid": "USAGE_INVALID",
    "unexpected_tool_call": "UNEXPECTED_TOOL_CALL",
    "host_failed": "HOST_FAILED",
}
_UNKNOWNS = {"host_connection_lost", "host_process_lost", "drain_timeout", "terminal_capture_ambiguous"}
_USAGE_KEYS = {"inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "totalTokens",
               "costInput", "costOutput", "costCacheRead", "costCacheWrite", "costTotal",
               "completeness", "finality"}
_REPLY_KEYS = {"protocol", "requestId", "nodeId", "resolvedModel", "turnsStarted", "durationMs",
               "budgetExhausted", "budgetOvershootTokens", "usage", "outcome", "stopReason", "result", "error"}

class WorkflowWireError(ValueError):
    pass

class CapabilityUnavailable(RuntimeError):
    code = "CAPABILITY_UNAVAILABLE"

def _integer(value: Any, name: str, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise WorkflowWireError(f"{name} must be an integer in [{low}, {high}]")
    return value

def _closed(value: Any, keys: set[str], name: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise WorkflowWireError(f"{name} must be a closed object with exactly {sorted(keys)}")
    return value

def _id(value: Any, name: str) -> str:
    if not isinstance(value, str) or not _ID.fullmatch(value):
        raise WorkflowWireError(f"{name} is invalid")
    return value

def _model(value: Any, name: str) -> str | None:
    if value is not None and (not isinstance(value, str) or len(value) > 512 or not _MODEL.fullmatch(value)):
        raise WorkflowWireError(f"{name} is invalid")
    return value

def validate_request(request: Any) -> dict[str, Any]:
    required = {"protocol", "requestId", "nodeId", "prompt", "model", "maxTurns", "maxResultUtf8Bytes", "drainTimeoutMs", "tools"}
    allowed = required | {"softTokenBudget"}
    if not isinstance(request, dict) or not required <= set(request) <= allowed:
        raise WorkflowWireError("request has missing or unknown fields")
    if request["protocol"] != REQUEST_PROTOCOL or request["tools"] != "none":
        raise WorkflowWireError("unsupported workflow protocol or tools policy")
    _id(request["requestId"], "requestId"); _id(request["nodeId"], "nodeId")
    prompt = request["prompt"]
    if not isinstance(prompt, str) or not 1 <= len(prompt) <= 262144:
        raise WorkflowWireError("prompt is invalid")
    _model(request["model"], "model")
    if request["maxTurns"] != 1 or isinstance(request["maxTurns"], bool):
        raise WorkflowWireError("maxTurns must be exactly 1")
    if "softTokenBudget" in request and request["softTokenBudget"] is not None:
        _integer(request["softTokenBudget"], "softTokenBudget", 1, 1_000_000)
    _integer(request["maxResultUtf8Bytes"], "maxResultUtf8Bytes", 1, 1_048_576)
    _integer(request["drainTimeoutMs"], "drainTimeoutMs", 1, 30_000)
    return dict(request)

def _usage(value: Any, finality: str) -> dict[str, Any]:
    usage = _closed(value, _USAGE_KEYS, "usage")
    for key in ("inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "totalTokens"):
        _integer(usage[key], f"usage.{key}", 0, _SAFE_INT)
    for key in ("costInput", "costOutput", "costCacheRead", "costCacheWrite", "costTotal"):
        cost = usage[key]
        if cost is not None and (isinstance(cost, bool) or not isinstance(cost, (int, float)) or not math.isfinite(cost) or not 0 <= cost <= 1e15):
            raise WorkflowWireError(f"usage.{key} is invalid")
    if usage["completeness"] != "complete_host_observation" or usage["finality"] != finality:
        raise WorkflowWireError("usage completeness/finality is invalid")
    return usage

def validate_reply(reply: Any, *, request: dict[str, Any]) -> dict[str, Any]:
    value = _closed(reply, _REPLY_KEYS, "reply")
    if value["protocol"] != REPLY_PROTOCOL or value["requestId"] != request["requestId"] or value["nodeId"] != request["nodeId"]:
        raise WorkflowWireError("reply correlation or protocol mismatch")
    _model(value["resolvedModel"], "resolvedModel")
    _integer(value["turnsStarted"], "turnsStarted", 0, 1)
    _integer(value["durationMs"], "durationMs", 0, _SAFE_INT)
    if not isinstance(value["budgetExhausted"], bool): raise WorkflowWireError("budgetExhausted must be boolean")
    _integer(value["budgetOvershootTokens"], "budgetOvershootTokens", 0, _SAFE_INT)
    outcome = value["outcome"]
    decoded_usage = _usage(value["usage"], "known_prefix" if outcome == "execution_unknown" else "final")
    budget = request.get("softTokenBudget")
    expected_exhausted = budget is not None and decoded_usage["totalTokens"] >= budget
    expected_overshoot = 0 if budget is None else max(0, decoded_usage["totalTokens"] - budget)
    if value["budgetExhausted"] != expected_exhausted or value["budgetOvershootTokens"] != expected_overshoot:
        raise WorkflowWireError("budget semantics mismatch")
    if outcome == "completed" and value["turnsStarted"] != 1:
        raise WorkflowWireError("completed requires one started turn")
    if outcome == "completed":
        if value["stopReason"] != "completed" or value["error"] is not None:
            raise WorkflowWireError("invalid completed variant")
        result = _closed(value["result"], {"text", "utf8Bytes", "sha256"}, "result")
        if not isinstance(result["text"], str): raise WorkflowWireError("result.text must be a string")
        raw = result["text"].encode("utf-8")
        if len(raw) > request["maxResultUtf8Bytes"] or result["utf8Bytes"] != len(raw) or result["sha256"] != hashlib.sha256(raw).hexdigest():
            raise WorkflowWireError("result bytes or digest mismatch")
    elif outcome == "failed":
        reason = value["stopReason"]
        if reason not in _FAILURES or value["result"] is not None: raise WorkflowWireError("invalid failed variant")
        error = _closed(value["error"], {"code", "message"}, "error")
        if error["code"] != _FAILURES[reason] or not isinstance(error["message"], str) or len(error["message"]) > 512:
            raise WorkflowWireError("invalid failure error")
    elif outcome == "cancelled":
        if value["stopReason"] != "caller_aborted" or value["result"] is not None or value["error"] is not None:
            raise WorkflowWireError("invalid cancelled variant")
    elif outcome == "execution_unknown":
        if value["stopReason"] not in _UNKNOWNS or value["result"] is not None: raise WorkflowWireError("invalid unknown variant")
        error = _closed(value["error"], {"code", "message"}, "error")
        if error["code"] != "EXECUTION_UNKNOWN" or not isinstance(error["message"], str) or len(error["message"]) > 512:
            raise WorkflowWireError("invalid unknown error")
    else:
        raise WorkflowWireError("unknown outcome")
    return dict(value)

async def run_agent(request: dict[str, Any]) -> dict[str, Any]:
    """Run the sole native Workflow V1 operation and await terminal settlement."""
    checked = validate_request(request)
    envelope = {"type": "workflow.run_agent", "request": checked}
    try:
        raw = await repl.host_request(envelope, cancel_on_cancel=True, drain_timeout_ms=checked["drainTimeoutMs"])
    except repl.HostRequestUnavailable as exc:
        raise CapabilityUnavailable(str(exc)) from exc
    except repl.HostConnectionLost:
        raw = {"status": "ok", "result": _unknown_reply(checked, "host_connection_lost", "authenticated host connection lost after possible dispatch")}
    except repl.HostDrainTimeout:
        raw = {"status": "ok", "result": _unknown_reply(checked, "drain_timeout", "host drain timed out after cancellation") }
    if not isinstance(raw, dict) or raw.get("status") != "ok" or set(raw) != {"status", "result"}:
        message = raw.get("error", "workflow.run_agent is unavailable") if isinstance(raw, dict) else "workflow.run_agent is unavailable"
        raise CapabilityUnavailable(str(message))
    return validate_reply(raw["result"], request=checked)

def _unknown_reply(request: dict[str, Any], reason: str, message: str) -> dict[str, Any]:
    usage = {"inputTokens": 0, "outputTokens": 0, "cacheReadTokens": 0, "cacheWriteTokens": 0,
             "totalTokens": 0, "costInput": None, "costOutput": None, "costCacheRead": None,
             "costCacheWrite": None, "costTotal": None, "completeness": "complete_host_observation", "finality": "known_prefix"}
    return {"protocol": REPLY_PROTOCOL, "requestId": request["requestId"], "nodeId": request["nodeId"],
            "resolvedModel": None, "turnsStarted": 0, "durationMs": 0, "budgetExhausted": False,
            "budgetOvershootTokens": 0, "usage": usage, "outcome": "execution_unknown",
            "stopReason": reason, "result": None,
            "error": {"code": "EXECUTION_UNKNOWN", "message": message}}
