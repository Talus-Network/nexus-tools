from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from deploy import render_plan, validate_config


class DeploymentPlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory(prefix="agent api deploy test ")
        self.root = Path(self.temp_dir.name)
        self.package = self.root / "packages" / "agent_api_rewards"
        self.package.mkdir(parents=True)
        (self.package / "Move.toml").write_text("[package]\nname = \"agent_api_rewards\"\n")
        self.config_path = self.root / "deployment.json"

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def coin(self, slug: str, package: str, object_id: str, published: bool = True) -> dict[str, object]:
        return {
            "coin_slug": slug,
            "package_name": package,
            "package_path": "./packages/agent_api_rewards",
            "coin_type": "0xabc::rewards::LOYALTY",
            "event_rpc_url": "https://rpc.example.invalid",
            "invocation_cost_mist": "0",
            "package_id": object_id if published else "",
            "cashier_id": "0x101" if published else "",
            "operator_cap_id": "0x103" if published else "",
            "credit_rate": "2" if published else "",
            "settlement_cap_id": "0x102" if published else "",
            "tool_witness_ids": (
                {operation: f"0x10{index}" for index, operation in enumerate(("register", "charge", "authorize", "revoke"), 1)}
                if published
                else {operation: "" for operation in ("register", "charge", "authorize", "revoke")}
            ),
        }

    def config(self, coins: list[dict[str, object]] | None = None) -> dict[str, object]:
        return {
            "schema_version": 1,
            "coins": coins or [self.coin("rewards", "agent_api_rewards", "0x100")],
            "service": {
                "public_url": "https://agent-api.example.invalid",
                "provider_url": "https://provider.example.invalid",
                "bind_addr": "127.0.0.1:8080",
                "database_path": "/var/lib/agent-api/agent-api.sqlite",
                "master_key_env": "AGENT_API_MASTER_KEY",
                "provider_operator_key_env": "AGENT_API_PROVIDER_OPERATOR_KEY",
                "toolkit_config_path": "/run/secrets/nexus-toolkit.json",
                "allowed_leaders_path": "/run/secrets/allowed-leaders.json",
                "query_signing_key_file": "/run/secrets/query-key.json",
                "retrieve_key_signing_key_file": "/run/secrets/retrieve-key.json",
                "http_tool_fqn_version": "42",
                "invocation_cost_mist": "0",
            },
            "worker": {
                "sui_grpc_url": "https://grpc.example.invalid",
                "settlement_signer_key_env": "AGENT_API_SETTLEMENT_SIGNER_KEY_B64",
                "settlement_gas_budget_mist": "50000000",
            },
        }

    def validate(self, config: dict[str, object], require_registered: bool = True):
        return validate_config(config, self.config_path.parent, require_registered=require_registered)

    def test_one_coin_plan_registers_four_concrete_tools_and_two_shared_http_tools(self) -> None:
        normalized, errors, pending = self.validate(self.config())
        self.assertEqual(errors, [])
        self.assertEqual(pending, [])

        plan = render_plan(normalized)

        self.assertEqual(plan.count("nexus tool register onchain"), 4)
        self.assertIn("xyz.taluslabs.agent_api.rewards.register@2", plan)
        self.assertIn("xyz.taluslabs.agent_api.rewards.charge@1", plan)
        self.assertIn("xyz.taluslabs.agent_api.rewards.authorize@1", plan)
        self.assertIn("xyz.taluslabs.agent_api.rewards.revoke@1", plan)
        self.assertIn("xyz.taluslabs.agent_api.query@42", plan)
        self.assertIn("xyz.taluslabs.agent_api.retrieve-key@42", plan)
        self.assertEqual(plan.count("nexus tool register offchain"), 1)
        self.assertIn("--batch", plan)
        self.assertIn('"module":"accounting"', plan)
        self.assertIn("0x100::accounting::set_credit_rate", plan)
        self.assertIn("@0x103", plan)
        self.assertIn("@0x101", plan)
        self.assertIn("<0xabc::rewards::LOYALTY>", plan)
        self.assertIn("@0x103 @0x101 2", plan)
        self.assertLess(plan.index("sui client ptb --move-call"), plan.index("nexus tool register onchain"))
        self.assertIn("Execution: NOT EXECUTED", plan)

    def test_plan_uses_the_fixed_runtime_secret_environment_names(self) -> None:
        normalized, errors, _ = self.validate(self.config())
        self.assertEqual(errors, [])

        plan = render_plan(normalized)

        self.assertIn("AGENT_API_MASTER_KEY=${AGENT_API_MASTER_KEY:", plan)
        self.assertIn("AGENT_API_PROVIDER_OPERATOR_KEY=${AGENT_API_PROVIDER_OPERATOR_KEY:", plan)
        self.assertIn(
            "AGENT_API_SETTLEMENT_SIGNER_KEY_B64=${AGENT_API_SETTLEMENT_SIGNER_KEY_B64:",
            plan,
        )

    def test_two_coin_plan_has_eight_onchain_tools_and_two_settlement_routes(self) -> None:
        second_package = self.root / "packages" / "agent_api_usdt"
        second_package.mkdir()
        (second_package / "Move.toml").write_text("[package]\nname = \"agent_api_usdt\"\n")
        coins = [
            self.coin("rewards", "agent_api_rewards", "0x100"),
            self.coin("usdt", "agent_api_usdt", "0x200"),
        ]
        coins[1]["package_path"] = "./packages/agent_api_usdt"
        coins[1]["cashier_id"] = "0x201"
        coins[1]["operator_cap_id"] = "0x203"
        coins[1]["credit_rate"] = "3"
        coins[1]["settlement_cap_id"] = "0x202"
        normalized, errors, pending = self.validate(self.config(coins))
        self.assertEqual(errors, [])
        self.assertEqual(pending, [])

        plan = render_plan(normalized)

        self.assertEqual(plan.count("nexus tool register onchain"), 8)
        self.assertEqual(plan.count("nexus tool register offchain"), 1)
        self.assertEqual(plan.count('"module":"accounting"'), 2)
        self.assertIn("xyz.taluslabs.agent_api.usdt.authorize@1", plan)
        self.assertEqual(plan.count("sui client ptb --move-call"), 2)
        self.assertIn("@0x203", plan)
        self.assertIn("@0x201", plan)
        self.assertIn(" 3", plan)

    def test_unpublished_package_plans_publication_and_defers_registration(self) -> None:
        config = self.config([self.coin("rewards", "agent_api_rewards", "0x100", published=False)])
        normalized, errors, pending = self.validate(config, require_registered=False)
        self.assertEqual(errors, [])
        self.assertTrue(any(item.endswith("package_id") for item in pending))

        plan = render_plan(normalized)

        self.assertIn("sui client publish", plan)
        self.assertIn("After publication", plan)
        self.assertIn("operator_cap_id", plan)
        self.assertIn("credit_rate", plan)
        self.assertNotIn("nexus tool register onchain", plan)

    def test_cli_plan_is_cwd_independent_and_never_runs_cli_commands(self) -> None:
        config = self.config([self.coin("rewards", "agent_api_rewards", "0x100", published=False)])
        self.config_path.write_text(json.dumps(config))
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        marker = self.root / "cli-was-executed"
        for command in ("sui", "nexus"):
            stub = fake_bin / command
            stub.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "from pathlib import Path\n"
                "Path(os.environ['CLI_SIDE_EFFECT_MARKER']).touch()\n"
            )
            stub.chmod(0o755)
        environment = os.environ.copy()
        environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
        environment["CLI_SIDE_EFFECT_MARKER"] = str(marker)

        result = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve().parents[1] / "deploy.py"),
                "plan",
                "--config",
                str(self.config_path),
            ],
            cwd="/",
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("sui client publish", result.stdout)
        self.assertIn("Execution: NOT EXECUTED", result.stdout)
        publish_line = next(
            line for line in result.stdout.splitlines() if line.startswith("NOT EXECUTED: sui client publish ")
        )
        publish_args = shlex.split(publish_line.removeprefix("NOT EXECUTED: "))
        self.assertEqual(Path(publish_args[-1]), self.package.resolve())
        self.assertFalse(marker.exists())

    def test_full_plan_paths_are_shell_safe_when_invoked_outside_bundle(self) -> None:
        config = self.config()
        service = config["service"]
        service["database_path"] = "runtime data/agent api.sqlite"
        service["toolkit_config_path"] = "secret files/toolkit config.json"
        service["allowed_leaders_path"] = "secret files/allowed leaders.json"
        service["query_signing_key_file"] = "secret files/query signing key.json"
        service["retrieve_key_signing_key_file"] = "secret files/retrieve signing key.json"
        self.config_path.write_text(json.dumps(config))
        fake_bin = self.root / "fake commands"
        fake_bin.mkdir()
        marker = self.root / "cli-was-executed"
        for command in ("sui", "nexus"):
            stub = fake_bin / command
            stub.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "from pathlib import Path\n"
                "Path(os.environ['CLI_SIDE_EFFECT_MARKER']).touch()\n"
            )
            stub.chmod(0o755)
        environment = os.environ.copy()
        environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
        environment["CLI_SIDE_EFFECT_MARKER"] = str(marker)

        result = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve().parents[1] / "deploy.py"),
                "plan",
                "--config",
                str(self.config_path),
            ],
            cwd="/",
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists())
        plan = result.stdout
        expected_database = (self.root / "runtime data/agent api.sqlite").resolve()
        expected_paths = {
            "AGENT_API_DB_PATH": expected_database,
            "NEXUS_TOOLKIT_CONFIG_PATH": (self.root / "secret files/toolkit config.json").resolve(),
        }
        for name, expected in expected_paths.items():
            assignment = next(line for line in plan.splitlines() if line.startswith(f"{name}="))
            tokens = shlex.split(assignment)
            self.assertEqual(len(tokens), 1)
            self.assertEqual(tokens[0].split("=", 1), [name, str(expected)])

        expected_key_paths = {
            (self.root / "secret files/query signing key.json").resolve(),
            (self.root / "secret files/retrieve signing key.json").resolve(),
        }
        keygen_paths = set()
        registered_key_paths = set()
        allowed_leaders_path = (self.root / "secret files/allowed leaders.json").resolve()
        for line in plan.splitlines():
            if line.startswith("NOT EXECUTED: nexus tool auth keygen "):
                args = shlex.split(line.removeprefix("NOT EXECUTED: "))
                keygen_paths.add(Path(args[args.index("--out") + 1]))
            elif line.startswith("NOT EXECUTED: nexus tool auth register-key "):
                args = shlex.split(line.removeprefix("NOT EXECUTED: "))
                registered_key_paths.add(Path(args[args.index("--signing-key") + 1]))
            elif line.startswith("NOT EXECUTED: nexus tool auth export-allowed-leaders "):
                args = shlex.split(line.removeprefix("NOT EXECUTED: "))
                self.assertEqual(Path(args[args.index("--out") + 1]), allowed_leaders_path)
        self.assertEqual(keygen_paths, expected_key_paths)
        self.assertEqual(registered_key_paths, expected_key_paths)

        mock_db_line = next(
            line for line in plan.splitlines() if line.strip().startswith("AGENT_API_MOCK_PROVIDER_DB=")
        )
        mock_db_tokens = shlex.split(mock_db_line.strip().removesuffix("\\"))
        self.assertEqual(
            mock_db_tokens[0].split("=", 1),
            ["AGENT_API_MOCK_PROVIDER_DB", str(expected_database.with_name("agent-api-mock-provider.sqlite"))],
        )

        manifest_paths = []
        for line in plan.splitlines():
            if "--manifest-path" not in line:
                continue
            args = shlex.split(line.strip().removesuffix("\\"))
            manifest_paths.append(Path(args[args.index("--manifest-path") + 1]))
        expected_manifest = Path(__file__).resolve().parents[1] / "offchain" / "Cargo.toml"
        self.assertEqual(manifest_paths, [expected_manifest] * 3)
        self.assertTrue(all(path.is_file() for path in manifest_paths))

    def test_cargo_commands_quote_and_resolve_manifest_under_bundle_path_with_spaces(self) -> None:
        bundle_root = self.root / "authority pattern bundle"
        offchain = bundle_root / "offchain"
        offchain.mkdir(parents=True)
        source_manifest = Path(__file__).resolve().parents[1] / "offchain" / "Cargo.toml"
        manifest = offchain / "Cargo.toml"
        shutil.copyfile(source_manifest, manifest)
        normalized, errors, pending = self.validate(self.config())
        self.assertEqual(errors, [])
        self.assertEqual(pending, [])

        with patch("deploy.BUNDLE_ROOT", bundle_root):
            plan = render_plan(normalized)

        manifest_paths = []
        for line in plan.splitlines():
            if "--manifest-path" not in line:
                continue
            args = shlex.split(line.strip().removesuffix("\\"))
            manifest_paths.append(Path(args[args.index("--manifest-path") + 1]))
        self.assertEqual(manifest_paths, [manifest] * 3)
        self.assertTrue(all(path.is_file() for path in manifest_paths))
        self.assertIn(shlex.quote(str(manifest)), plan)

    def test_relative_package_path_is_resolved_from_config_not_current_directory(self) -> None:
        config = self.config()
        original_cwd = Path.cwd()
        try:
            os.chdir("/")
            normalized, errors, _ = self.validate(config)
        finally:
            os.chdir(original_cwd)
        self.assertEqual(errors, [])
        self.assertEqual(normalized["coins"][0]["package_path"], self.package.resolve())

    def test_secret_values_are_rejected_and_never_echoed(self) -> None:
        config = self.config()
        sentinel = "never-print-this-provider-secret"
        config["service"]["provider_operator_key"] = sentinel
        normalized, errors, _ = self.validate(config, require_registered=False)
        self.assertTrue(any("unsupported field" in error for error in errors))
        self.assertNotIn(sentinel, "\n".join(errors))
        del config["service"]["provider_operator_key"]
        normalized, errors, _ = self.validate(config)
        self.assertEqual(errors, [])
        prior_values = {
            name: os.environ.get(name)
            for name in ("AGENT_API_MASTER_KEY", "AGENT_API_PROVIDER_OPERATOR_KEY")
        }
        os.environ["AGENT_API_MASTER_KEY"] = sentinel
        os.environ["AGENT_API_PROVIDER_OPERATOR_KEY"] = sentinel
        try:
            self.assertNotIn(sentinel, render_plan(normalized))
        finally:
            for name, prior in prior_values.items():
                if prior is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = prior

    def test_noncanonical_secret_environment_names_are_rejected(self) -> None:
        cases = (
            ("service", "master_key_env", "CUSTOM_MASTER_KEY", "AGENT_API_MASTER_KEY"),
            (
                "service",
                "provider_operator_key_env",
                "CUSTOM_PROVIDER_KEY",
                "AGENT_API_PROVIDER_OPERATOR_KEY",
            ),
            (
                "worker",
                "settlement_signer_key_env",
                "CUSTOM_SETTLEMENT_SIGNER",
                "AGENT_API_SETTLEMENT_SIGNER_KEY_B64",
            ),
        )
        for section, field, custom_name, runtime_name in cases:
            with self.subTest(field=f"{section}.{field}"):
                config = self.config()
                config[section][field] = custom_name

                _, errors, _ = self.validate(config)

                self.assertIn(
                    f"{section}.{field}: runtime reads the fixed variable {runtime_name}",
                    errors,
                )

    def test_invalid_object_id_and_missing_post_publication_ids_fail_validation(self) -> None:
        config = self.config()
        config["coins"][0]["tool_witness_ids"]["authorize"] = "not-an-object-id"
        normalized, errors, _ = self.validate(config)
        self.assertTrue(any("tool_witness_ids.authorize" in error for error in errors))

        unpublished = self.config([self.coin("rewards", "agent_api_rewards", "0x100", published=False)])
        _, errors, _ = self.validate(unpublished)
        self.assertTrue(any("required after publication" in error for error in errors))

    def test_missing_cashier_pricing_defers_setup_and_tool_registration(self) -> None:
        config = self.config()
        config["coins"][0]["operator_cap_id"] = ""
        config["coins"][0]["credit_rate"] = ""

        normalized, errors, pending = self.validate(config, require_registered=False)
        self.assertEqual(errors, [])
        self.assertIn("coins[rewards].operator_cap_id", pending)
        self.assertIn("coins[rewards].credit_rate", pending)

        plan = render_plan(normalized)
        self.assertIn("Cashier pricing setup", plan)
        self.assertNotIn("sui client ptb --move-call", plan)
        self.assertNotIn("nexus tool register onchain", plan)

    def test_credit_rate_must_be_a_positive_u64(self) -> None:
        for rate in ("0", "-1", "1.5", "18446744073709551616"):
            with self.subTest(rate=rate):
                config = self.config()
                config["coins"][0]["credit_rate"] = rate

                _, errors, _ = self.validate(config)

                self.assertIn("coins[0].credit_rate: expected a positive decimal u64", errors)


if __name__ == "__main__":
    unittest.main()
