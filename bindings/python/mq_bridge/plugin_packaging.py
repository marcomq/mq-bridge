"""Build a platform wheel for an mq-bridge native plugin package."""

from __future__ import annotations

import argparse
import importlib.util
import json
import shutil
import subprocess
import sys
import sysconfig
from pathlib import Path


def run(*command: str, cwd: Path) -> None:
    print("$", " ".join(command), flush=True)
    subprocess.run(command, cwd=cwd, check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="plugin repository root")
    parser.add_argument("--package", required=True, help="Python package directory")
    parser.add_argument("--out", default="dist", help="wheel output directory")
    args = parser.parse_args()

    missing = [module for module in ("build", "wheel") if importlib.util.find_spec(module) is None]
    if missing:
        parser.error(
            "plugin packaging requires mq-bridge[plugin-packaging] "
            f"(missing: {', '.join(missing)})"
        )

    root = Path(args.root).resolve()
    package = (root / args.package).resolve()
    manifest_path = package / "mq-bridge-plugin.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    library = manifest.get("library")
    if not isinstance(library, str):
        raise SystemExit(f"{manifest_path} must contain a string field 'library'")

    names = (f"lib{library}.so", f"lib{library}.dylib", f"{library}.dll")
    run("cargo", "build", "--release", cwd=root)
    built = next(
        (
            root / "target" / "release" / name
            for name in names
            if (root / "target" / "release" / name).is_file()
        ),
        None,
    )
    if built is None:
        raise SystemExit("cargo produced no shared plugin library")
    for name in names:
        (package / name).unlink(missing_ok=True)
    shutil.copy2(built, package / built.name)

    output = (root / args.out).resolve()
    project = package.parent
    existing = (
        {wheel: (wheel.stat().st_mtime_ns, wheel.stat().st_size) for wheel in output.glob("*.whl")}
        if output.is_dir()
        else {}
    )
    run(sys.executable, "-m", "build", "--wheel", "--outdir", str(output), cwd=project)
    wheels = [
        wheel
        for wheel in output.glob("*-py3-none-any.whl")
        if existing.get(wheel) != (wheel.stat().st_mtime_ns, wheel.stat().st_size)
    ]
    if not wheels:
        raise SystemExit(f"no wheel produced in {output}")
    wheel_path = max(wheels, key=lambda path: path.stat().st_mtime)
    platform_tag = sysconfig.get_platform().replace("-", "_").replace(".", "_")
    run(
        sys.executable,
        "-m",
        "wheel",
        "tags",
        "--platform-tag",
        platform_tag,
        "--remove",
        str(wheel_path),
        cwd=root,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
