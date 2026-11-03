import json
import platform
import sys
from pathlib import Path
from typing import Union

from ._mq_bridge import (
    Consumer,
    MemoryDrainer,
    Message,
    NonRetryableError,
    Publisher,
    RetryableError,
    Route,
    __version__,
    config_schema,
    init_logging,
    is_shutdown_requested,
    load_endpoint_plugin,
    register_endpoint,
    register_middleware,
    request_shutdown,
    unregister_endpoint,
    unregister_middleware,
)


def _plugin_platform_tag() -> str:
    machine = platform.machine().lower()
    arch = {"aarch64": "arm64", "amd64": "x64", "x86_64": "x64"}.get(machine, machine)
    suffix = "-gnu" if sys.platform == "linux" else "-msvc" if sys.platform == "win32" else ""
    return f"{sys.platform}-{arch}{suffix}"


def plugin_library_path(package: Union[str, Path]) -> str:
    """Resolve the native library in an installed mq-bridge plugin package."""
    root = Path(package).resolve()
    if root.is_file():
        root = root.parent
    manifest_path = root / "mq-bridge-plugin.json"
    if not manifest_path.is_file():
        raise FileNotFoundError(f"mq-bridge plugin manifest not found: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    try:
        library = manifest["library"]
        name = manifest["name"]
    except KeyError as error:
        raise ValueError(f"{manifest_path} must contain 'name' and 'library'") from error
    if not isinstance(name, str) or not isinstance(library, str):
        raise ValueError(f"{manifest_path} must contain string fields 'name' and 'library'")
    file_name = (
        f"{library}.dll"
        if sys.platform == "win32"
        else f"lib{library}.dylib"
        if sys.platform == "darwin"
        else f"lib{library}.so"
    )
    candidates = (root / "prebuilds" / _plugin_platform_tag() / file_name, root / file_name)
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    raise FileNotFoundError(
        f"no native library for {_plugin_platform_tag()} in mq-bridge plugin package {root}; "
        f"looked in {', '.join(map(str, candidates))}"
    )


def load_plugin_package(package: Union[str, Path]) -> str:
    """Resolve and load the native library in an mq-bridge plugin package."""
    return load_endpoint_plugin(plugin_library_path(package))

__all__ = [
    "Consumer",
    "MemoryDrainer",
    "Message",
    "NonRetryableError",
    "Publisher",
    "RetryableError",
    "Route",
    "__version__",
    "config_schema",
    "init_logging",
    "is_shutdown_requested",
    "load_endpoint_plugin",
    "load_plugin_package",
    "plugin_library_path",
    "register_endpoint",
    "register_middleware",
    "request_shutdown",
    "unregister_endpoint",
    "unregister_middleware",
]
