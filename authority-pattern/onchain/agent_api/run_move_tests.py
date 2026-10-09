#!/usr/bin/env python3
"""Build and test fixture packages against an explicitly supplied Nexus Sui tree."""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


BUNDLE_ROOT = Path(__file__).resolve().parents[3]
FIXTURE_ROOT = Path(__file__).resolve().parent / "fixtures"
NEXUS_DEPENDENCIES = {
    "nexus_primitives": "primitives",
    "nexus_interface": "interface",
    "nexus_kernel": "kernel",
    "nexus_registry": "registry",
    "nexus_tool": "tool",
    "nexus_workflow": "workflow",
    "nexus_scheduler": "scheduler",
}


class MoveTestError(ValueError):
    """A local dependency root or fixture manifest is invalid."""


def local_dependency_paths(sui_root: Path) -> dict[str, Path]:
    root = sui_root.expanduser().resolve()
    missing = [name for name in NEXUS_DEPENDENCIES.values() if not (root / name / "Move.toml").is_file()]
    if missing:
        expected = ", ".join(missing)
        raise MoveTestError(f"--sui-root must contain these Nexus Move packages: {expected}")
    return {alias: root / directory for alias, directory in NEXUS_DEPENDENCIES.items()}


def rewrite_manifest(manifest: Path, dependencies: dict[str, Path]) -> None:
    text = manifest.read_text(encoding="utf-8")
    for alias, path in dependencies.items():
        pattern = re.compile(
            rf"(?m)^([ \t]*){re.escape(alias)}[ \t]*=[ \t]*"
            rf"\{{[ \t]*r\.mvr[ \t]*=[ \t]*\"[^\"]+\"[ \t]*\}}[ \t]*$"
        )
        replacement = rf'\1{alias} = {{ local = "{path.as_posix()}" }}'
        text, count = pattern.subn(replacement, text)
        if count != 1:
            raise MoveTestError(
                f"{manifest}: expected one published MVR dependency for {alias}, found {count}"
            )
    manifest.write_text(text, encoding="utf-8")


def prepare_fixture(source: Path, destination: Path, dependencies: dict[str, Path]) -> Path:
    shutil.copytree(source, destination)
    rewrite_manifest(destination / "Move.toml", dependencies)
    return destination


def run_fixture(sui: str, source: Path, dependencies: dict[str, Path], temp_root: Path) -> None:
    name = source.name
    package = prepare_fixture(source, temp_root / name, dependencies)
    print(f"Building and testing {name} with explicit local Nexus Move dependencies.")
    subprocess.run([sui, "move", "build", "--path", str(package)], check=True, cwd=BUNDLE_ROOT)
    subprocess.run(
        [sui, "move", "test", "--path", str(package), "--coverage"], check=True, cwd=BUNDLE_ROOT
    )
    subprocess.run(
        [sui, "move", "coverage", "summary", "--path", str(package)], check=True, cwd=BUNDLE_ROOT
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sui-root",
        type=Path,
        required=True,
        help="explicit path to the Nexus repository's Sui package directory",
    )
    parser.add_argument("--sui-bin", default="sui", help="Sui CLI executable (default: sui)")
    args = parser.parse_args(argv)
    try:
        dependencies = local_dependency_paths(args.sui_root)
        with tempfile.TemporaryDirectory(prefix="authority-pattern-move-") as directory:
            temp_root = Path(directory)
            for fixture_name in ("sui", "test-coin"):
                run_fixture(args.sui_bin, FIXTURE_ROOT / fixture_name, dependencies, temp_root)
    except (MoveTestError, OSError, subprocess.CalledProcessError) as error:
        print(f"local Move verification failed: {error}", file=sys.stderr)
        return 1
    print("Both fixture suites passed in temporary local-dependency copies; no package was published.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
