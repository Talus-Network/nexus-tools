from __future__ import annotations

import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from run_move_tests import (
    FIXTURE_ROOT,
    NEXUS_DEPENDENCIES,
    MoveTestError,
    local_dependency_paths,
    prepare_fixture,
)


class LocalMoveRunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory(prefix="agent-api-move-runner-test-")
        self.root = Path(self.temp_dir.name)
        self.sui_root = self.root / "sui"
        for directory in NEXUS_DEPENDENCIES.values():
            package = self.sui_root / directory
            package.mkdir(parents=True)
            (package / "Move.toml").write_text("[package]\nname = \"fixture\"\n")

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_explicit_sui_root_maps_all_seven_local_packages(self) -> None:
        paths = local_dependency_paths(self.sui_root)
        self.assertEqual(set(paths), set(NEXUS_DEPENDENCIES))
        self.assertEqual(paths["nexus_primitives"], (self.sui_root / "primitives").resolve())
        self.assertEqual(paths["nexus_scheduler"], (self.sui_root / "scheduler").resolve())

    def test_fixture_copy_uses_local_paths_and_preserves_checked_in_manifest(self) -> None:
        source = FIXTURE_ROOT / "sui"
        original_manifest = (source / "Move.toml").read_text()
        destination = prepare_fixture(
            source,
            self.root / "temporary-sui-fixture",
            local_dependency_paths(self.sui_root),
        )
        rewritten = (destination / "Move.toml").read_text()
        manifest = tomllib.loads(rewritten)
        dependencies = {**manifest["dependencies"], **manifest["dev-dependencies"]}

        self.assertNotIn("r.mvr", rewritten)
        self.assertEqual(
            dependencies["nexus_primitives"]["local"], str((self.sui_root / "primitives").resolve())
        )
        self.assertEqual(
            dependencies["nexus_scheduler"]["local"], str((self.sui_root / "scheduler").resolve())
        )
        self.assertEqual((source / "Move.toml").read_text(), original_manifest)

    def test_wrong_sui_root_fails_before_running_sui(self) -> None:
        with self.assertRaisesRegex(MoveTestError, "Nexus Move packages"):
            local_dependency_paths(self.root)


if __name__ == "__main__":
    unittest.main()
