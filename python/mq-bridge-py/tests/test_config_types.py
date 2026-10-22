"""Tests for the schema surface and generated config types."""

import importlib.util
from pathlib import Path

import mq_bridge


PACKAGE_DIR = Path(__file__).resolve().parents[1]


def _load_generator():
    path = PACKAGE_DIR / "scripts" / "gen_config_types.py"
    spec = importlib.util.spec_from_file_location("gen_config_types", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_config_schema_is_generated_from_models() -> None:
    schema = mq_bridge.config_schema()

    assert schema["title"] == "Map_of_Route"
    assert "Endpoint" in schema["$defs"]
    assert "Route" in schema["$defs"]


def test_generated_config_types_are_up_to_date() -> None:
    """If this fails, re-run: uv run python scripts/gen_config_types.py"""
    generator = _load_generator()
    pyi, py = generator.generate()

    regen = (
        "stale generated config types. Regenerate:\n"
        "    cargo test --features schema --lib schema_tests::generate_json_schema\n"
        "    cd python/mq-bridge-py && uv run --no-sync python "
        "scripts/gen_config_types.py --schema ../../mq-bridge.schema.json"
    )
    assert pyi == (PACKAGE_DIR / "mq_bridge" / "config.pyi").read_text(), (
        f"config.pyi is {regen}"
    )
    assert py == (PACKAGE_DIR / "mq_bridge" / "config.py").read_text(), (
        f"config.py is {regen}"
    )


def test_config_types_module_exposes_expected_names() -> None:
    from mq_bridge import config

    for name in ("ConfigDocument", "RouteConfig", "EndpointConfig", "Endpoint", "Route"):
        assert hasattr(config, name)
