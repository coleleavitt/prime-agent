"""Prime Agent wasm-plugins skill: list and call the user's WebAssembly plugins.

The plugins run host-side, sandboxed; these functions are thin wrappers over
the generic host bridge (`rlm.host_request`). They only work inside the Prime
Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


async def list() -> Any:  # noqa: A001 - the skill's documented name
    """The session's loaded plugins.

    `[{"name", "description", "path", "tools", "commands"}, ...]`; each tool
    is `{"name", "description", "signature", "parameters"}`.
    """
    return await host_request("plugin.list")


async def tools() -> Any:
    """Every loaded plugin's tools, flattened.

    `[{"plugin", "name", "description", "signature", "parameters"}, ...]`;
    call one with `call(plugin, name, **args)`.
    """
    plugins = await host_request("plugin.list")
    return [{"plugin": plugin["name"], **tool} for plugin in plugins for tool in plugin.get("tools", [])]


async def call(name: str, method: str, **data: Any) -> Any:
    """Call plugin `name`'s `method` (one of its tools) with `data` as a JSON object.

    Returns the plugin's JSON answer. A declared tool's arguments are checked
    against its schema first. Raises when the plugin is not loaded, the
    arguments do not match, the plugin does not know the method, or it fails.
    """
    if not isinstance(name, str) or not name.strip():
        raise TypeError("name must be a non-empty str")
    if not isinstance(method, str) or not method.strip():
        raise TypeError("method must be a non-empty str")
    return await host_request(
        "plugin.call", {"plugin": name, "method": method, "data": data}
    )
