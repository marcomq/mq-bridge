"""Packaging guards: keep the two distribution manifests from silently drifting
and confirm the version is exported on the Python surface."""

import copy
from pathlib import Path

import pytest

import mq_bridge


PACKAGE_DIR = Path(__file__).resolve().parents[1]

# The full and basic wheels share one source tree; they are allowed to differ
# only in these keys. Anything else diverging is an accident waiting to ship.
ALLOWED_DIVERGENT = {
    ("project", "name"),
    ("project", "description"),
    ("project", "keywords"),
    ("tool", "maturin", "features"),
}


def _normalise(data: dict) -> dict:
    data = copy.deepcopy(data)
    for path in ALLOWED_DIVERGENT:
        node = data
        for key in path[:-1]:
            node = node.get(key, {})
        node.pop(path[-1], None)
    return data


def test_version_is_exported() -> None:
    assert isinstance(mq_bridge.__version__, str)
    assert mq_bridge.__version__


def test_pyproject_variants_only_differ_in_allowed_keys() -> None:
    tomllib = pytest.importorskip("tomllib")  # stdlib on Python >= 3.11

    full = tomllib.loads((PACKAGE_DIR / "pyproject.toml").read_text())
    basic = tomllib.loads((PACKAGE_DIR / "pyproject-basic.toml").read_text())

    assert _normalise(full) == _normalise(basic)

    # The keys we allow to differ should actually differ, so the guard stays
    # meaningful (otherwise the two files are simply identical copies).
    assert full["project"]["name"] != basic["project"]["name"]
    assert full["tool"]["maturin"]["features"] != basic["tool"]["maturin"]["features"]
