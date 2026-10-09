from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def _extract_step_script(workflow_path: Path, step_name: str) -> str:
    lines = workflow_path.read_text(encoding="utf-8").splitlines()
    for index, line in enumerate(lines):
        if line.strip() != f"- name: {step_name}":
            continue
        for run_index in range(index + 1, len(lines)):
            if lines[run_index].strip() != "run: |":
                if lines[run_index].strip().startswith("- name:"):
                    break
                continue
            following = lines[run_index + 1]
            script_indent = len(following) - len(following.lstrip())
            script_lines = []
            for script_line in lines[run_index + 1 :]:
                if script_line.strip():
                    indent = len(script_line) - len(script_line.lstrip())
                    if indent < script_indent:
                        break
                    script_lines.append(script_line[script_indent:])
                else:
                    script_lines.append("")
            return "\n".join(script_lines)
    raise AssertionError(f"workflow step {step_name!r} with a literal run block was not found")


def _job_section(workflow_text: str, job_name: str, next_job: str) -> str:
    start = workflow_text.index(f"  {job_name}:\n")
    end = workflow_text.index(f"  {next_job}:\n", start)
    return workflow_text[start:end]


class WorkflowContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory(prefix="agent api workflow tests ")
        self.root = Path(self.temp_dir.name)
        self._write_tool(
            "authority-pattern/offchain",
            json.loads((ROOT / "authority-pattern/offchain/tools.json").read_text(encoding="utf-8")),
            (ROOT / "authority-pattern/offchain/README.md").read_text(encoding="utf-8"),
        )
        self._write_tool(
            "offchain/tools/bench",
            json.loads((ROOT / "offchain/tools/bench/tools.json").read_text(encoding="utf-8")),
            "Internal benchmark guide.\n",
        )
        self._write_tool(
            "offchain/tools/ordinary-directory",
            {"tool_name": "ordinary", "command": "ordinary"},
            "Ordinary Tool guide.\n",
        )
        self._git("init", "--quiet")
        self._git("config", "user.name", "Workflow Contract Test")
        self._git("config", "user.email", "workflow-contract-test@example.invalid")
        self._git("config", "commit.gpgsign", "false")
        self._git("config", "core.hooksPath", "/dev/null")
        self._git("add", ".")
        self._git("commit", "--quiet", "-m", "workflow fixtures")

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def _write_tool(self, relative_dir: str, manifest: dict[str, object], readme: str) -> None:
        tool_dir = self.root / relative_dir
        tool_dir.mkdir(parents=True, exist_ok=True)
        (tool_dir / "tools.json").write_text(json.dumps(manifest), encoding="utf-8")
        (tool_dir / "README.md").write_text(readme, encoding="utf-8")
        (tool_dir / "src").mkdir(exist_ok=True)
        (tool_dir / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")

    def _git(self, *arguments: str) -> None:
        subprocess.run(
            ["git", *arguments],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        )

    def _run_discovery(self, *, changed_files: str, blocked_tools: str = "") -> dict[str, object]:
        output_path = self.root / "github-output.txt"
        environment = os.environ.copy()
        environment.update(
            {
                "CHANGED_FILES": changed_files,
                "BLOCKED_TOOLS": blocked_tools,
                "GITHUB_OUTPUT": str(output_path),
            }
        )
        script = _extract_step_script(
            ROOT / ".github/workflows/offchain-tools.discover.yml", "Build matrices"
        )
        result = subprocess.run(
            ["bash", "-e", "-u", "-o", "pipefail", "-c", script],
            cwd=self.root,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        outputs = dict(
            line.split("=", 1)
            for line in output_path.read_text(encoding="utf-8").splitlines()
        )
        return {
            name: json.loads(value) if name.startswith("matrix-") else value
            for name, value in outputs.items()
        }

    def test_discovery_excludes_deploy_false_from_automatic_matrices(self) -> None:
        matrices = self._run_discovery(changed_files="")

        self.assertEqual(
            [entry["tool"] for entry in matrices["matrix-all"]["include"]],
            ["ordinary"],
        )
        self.assertEqual(
            [entry["tool"] for entry in matrices["matrix-changed"]["include"]],
            ["ordinary"],
        )

    def test_repo_blocklist_still_excludes_blocked_tools_from_matrices(self) -> None:
        matrices = self._run_discovery(changed_files="", blocked_tools="ordinary")

        self.assertEqual(matrices["matrix-all"]["include"], [])
        self.assertEqual(matrices["matrix-changed"]["include"], [])

    def test_dispatch_inputs_and_automatic_ci_jobs_use_the_filtered_outputs(self) -> None:
        ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        deploy = _job_section(ci, "deploy", "prepare")
        prepare = _job_section(ci, "prepare", "register")
        readiness = _job_section(ci, "readiness", "trigger-tf-apply")

        self.assertIn("&& needs.discover.outputs.matrix-changed", deploy)
        self.assertIn("needs.discover.outputs.matrix-all", deploy)
        self.assertIn("github.event_name == 'pull_request'", deploy)
        self.assertIn("github.ref_name == 'main'", deploy)
        self.assertIn("|| needs.discover.outputs.matrix-all", deploy)
        self.assertIn("inputs.pr-number != ''", prepare)
        self.assertIn("inputs.target-env != ''", prepare)
        self.assertIn("matrix-json: ${{ needs.discover.outputs.matrix-all }}", prepare)
        self.assertIn("needs.discover.outputs.matrix-changed", readiness)

        dispatch_matrix = self._run_discovery(changed_files="")["matrix-all"]["include"]
        self.assertNotIn("agent-api", {entry["tool"] for entry in dispatch_matrix})

    def test_discovery_and_coverage_hooks_still_include_bundle_validation(self) -> None:
        pre_commit = (ROOT / ".pre-commit/15-authority-pattern").read_text(encoding="utf-8")
        root_justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        coverage = (ROOT / ".github/workflows/coverage.yaml").read_text(encoding="utf-8")
        coverage_action = (ROOT / ".github/actions/coverage/action.yml").read_text(encoding="utf-8")

        self.assertIn("just authority-tool test", pre_commit)
        self.assertIn("mod authority-tool 'authority-pattern/justfile'", root_justfile)
        self.assertIn("authority-pattern/offchain/**", coverage)
        self.assertIn("--manifest-path ../authority-pattern/offchain/Cargo.toml", coverage_action)

    def test_docs_sync_preserves_tool_directory_and_maps_agent_api(self) -> None:
        self._write_tool(
            "offchain/tools/storage-walrus",
            json.loads((ROOT / "offchain/tools/storage-walrus/tools.json").read_text(encoding="utf-8")),
            (ROOT / "offchain/tools/storage-walrus/README.md").read_text(encoding="utf-8"),
        )
        docs_repo = self.root / "gitbook-docs"
        docs_repo.mkdir()
        subprocess.run(["git", "init", "--quiet"], cwd=docs_repo, check=True)
        script = _extract_step_script(ROOT / ".github/workflows/sync_docs.yml", "Sync documentation")
        script = script.replace("${{ env.REPO_NAME }}", "nexus-tools")

        result = subprocess.run(
            ["bash", "-e", "-u", "-o", "pipefail", "-c", script],
            cwd=self.root,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        self.assertEqual(
            (docs_repo / "tools/agent-api/README.md").read_text(encoding="utf-8"),
            (self.root / "authority-pattern/offchain/README.md").read_text(encoding="utf-8"),
        )
        self.assertTrue((docs_repo / "tools/ordinary-directory/README.md").is_file())
        self.assertFalse((docs_repo / "tools/ordinary").exists())
        self.assertEqual(
            (docs_repo / "tools/storage-walrus/README.md").read_text(encoding="utf-8"),
            (ROOT / "offchain/tools/storage-walrus/README.md").read_text(encoding="utf-8"),
        )
        self.assertFalse((docs_repo / "tools/walrus").exists())
        self.assertFalse((docs_repo / "tools/bench").exists())
        self.assertFalse((docs_repo / "tools/offchain").exists())


if __name__ == "__main__":
    unittest.main()
