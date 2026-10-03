"""Platform dispatch and lazy macOS framework loading for computer use."""

from __future__ import annotations

import os
import shutil
import stat
import sys
from functools import cache
from importlib import import_module
from types import ModuleType
from typing import NamedTuple

from .errors import ComputerUseError


class MacFrameworks(NamedTuple):
    """The three macOS frameworks computer use drives."""

    cocoa: ModuleType
    quartz: ModuleType
    app_services: ModuleType


def _backend() -> str | None:
    """Report the available backend: "mac" on darwin, "wayland" under a niri
    Wayland session, "linux" (X11) when the xdotool tool is on PATH, otherwise
    None.

    The Wayland check runs ahead of the X11 one: a niri session usually also
    exports DISPLAY through xwayland-satellite, but its native windows are not
    X11 windows, so X11 tooling would see only the XWayland clients.
    """
    if sys.platform == "darwin":
        return "mac"
    if _niri_session():
        return "wayland"
    if shutil.which("xdotool") is not None:
        return "linux"
    return None


def _niri_session() -> bool:
    """Report whether this process runs under niri: WAYLAND_DISPLAY plus a live NIRI_SOCKET."""
    if not os.environ.get("WAYLAND_DISPLAY"):
        return False
    path = os.environ.get("NIRI_SOCKET") or ""
    if not path:
        return False
    try:
        return stat.S_ISSOCK(os.stat(path).st_mode)
    except OSError:
        return False


@cache
def _require_mac() -> MacFrameworks:
    """Import the Cocoa, Quartz, and ApplicationServices frameworks lazily.

    The frameworks load only on darwin and the result is cached per process.
    Raises ComputerUseError TRANSPORT_ERROR off darwin or when a framework is
    missing, naming the missing backend.
    """
    if sys.platform != "darwin":
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            f"computer use backend unavailable: the macOS frameworks need darwin, this host runs {sys.platform}",
        )
    try:
        cocoa = import_module("Cocoa")
        quartz = import_module("Quartz")
        app_services = import_module("ApplicationServices")
    except ImportError as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            f"computer use backend unavailable: the macOS frameworks are not installed ({error})",
        ) from error
    return MacFrameworks(cocoa=cocoa, quartz=quartz, app_services=app_services)


def _require_linux() -> ModuleType:
    """Import the Linux X11 backend module lazily for the Linux lane.

    Raises ComputerUseError TRANSPORT_ERROR when the X11 tools are missing or
    the backend module has not shipped yet.
    """
    if _backend() != "linux":
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: the Linux _backend needs the xdotool tool on PATH",
        )
    try:
        from . import _linux
    except ImportError as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: the Linux _backend module is not installed yet",
        ) from error
    return _linux


def _require_wayland() -> ModuleType:
    """Import the Wayland (niri) backend module lazily for the Wayland lane.

    Raises ComputerUseError TRANSPORT_ERROR outside a niri Wayland session.
    """
    if _backend() != "wayland":
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: the Wayland _backend needs a niri session "
            "(WAYLAND_DISPLAY and a live NIRI_SOCKET)",
        )
    from . import _wayland

    return _wayland
