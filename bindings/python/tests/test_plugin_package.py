import json
import platform
import sys
from pathlib import Path

import pytest

from mq_bridge import _plugin_platform_tag, plugin_library_path


def test_plugin_library_path_selects_current_platform_prebuild(tmp_path: Path) -> None:
    (tmp_path / "mq-bridge-plugin.json").write_text(
        json.dumps({"name": "reference", "library": "reference"}), encoding="utf-8"
    )
    prebuild = tmp_path / "prebuilds" / _plugin_platform_tag()
    prebuild.mkdir(parents=True)
    libraries = [prebuild / name for name in ("reference.dll", "libreference.dylib", "libreference.so")]
    for library in libraries:
        library.write_bytes(b"fixture")

    platform_index = 0 if sys.platform == "win32" else 1 if sys.platform == "darwin" else 2
    assert Path(plugin_library_path(tmp_path)) == libraries[platform_index]


def test_platform_tag_matches_prebuild_convention() -> None:
    if sys.platform == "linux" and platform.machine().lower() == "x86_64":
        assert _plugin_platform_tag() == "linux-x64-gnu"


def test_plugin_library_path_reports_missing_manifest(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError, match="plugin manifest not found"):
        plugin_library_path(tmp_path)
