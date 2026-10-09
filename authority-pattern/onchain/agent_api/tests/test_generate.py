import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from generate import ConfigError, generate, render_package, validate_config


FIXTURES = ROOT / "fixtures"
CONFIGS = FIXTURES / "config"


class GeneratorTests(unittest.TestCase):
    def load_config(self, name):
        return json.loads((CONFIGS / f"{name}.json").read_text(encoding="utf-8"))

    def test_checked_in_coin_fixtures_match_deterministic_generation(self):
        for name in ("sui", "test-coin"):
            with self.subTest(name=name):
                expected = render_package(self.load_config(name))
                package = FIXTURES / name
                self.assertEqual(
                    {path: (package / path).read_text(encoding="utf-8") for path in expected},
                    expected,
                )

    def test_coin_types_are_configuration_driven(self):
        sui = validate_config(self.load_config("sui"))
        test_coin = validate_config(self.load_config("test-coin"))
        self.assertEqual(sui["coin_type"], "0x2::sui::SUI")
        self.assertEqual(test_coin["coin_type"], "agent_api_test_coin::test_coin::TEST_COIN")
        self.assertNotEqual(sui["coin_type"], test_coin["coin_type"])

    def test_accepts_concrete_nested_generic_coin_types(self):
        config = self.load_config("sui")
        configured = {
            **config,
            "named_addresses": {"payments": "0x123"},
            "coin_type": "payments::pool::LP<0x2::sui::SUI, vector<u8>>",
        }
        self.assertEqual(
            validate_config(configured)["coin_type"],
            "payments::pool::LP<0x2::sui::SUI, vector<u8>>",
        )

    def test_rejects_unknown_nested_generic_addresses(self):
        config = self.load_config("sui")
        with self.assertRaisesRegex(ConfigError, "configured named address"):
            validate_config({**config, "coin_type": "0x2::pool::LP<unknown::coin::COIN>"})

    def test_rejects_unknown_or_malformed_coin_types(self):
        config = self.load_config("sui")
        for bad_type in ("SUI", "0x2::sui", "0x2::sui::SUI<T>", "0x2::Sui::SUI"):
            with self.subTest(bad_type=bad_type):
                with self.assertRaises(ConfigError):
                    validate_config({**config, "coin_type": bad_type})

    def test_rejects_unconfigured_named_coin_address(self):
        config = self.load_config("sui")
        with self.assertRaisesRegex(ConfigError, "configured named address"):
            validate_config({**config, "coin_type": "unknown::coin::COIN"})

    def test_rejects_invalid_or_duplicate_dependency_aliases(self):
        config = self.load_config("sui")
        with self.assertRaises(ConfigError):
            validate_config({**config, "dependencies": [{"alias": "../coin", "mvr": "@x/coin"}]})
        with self.assertRaisesRegex(ConfigError, "duplicate or reserved"):
            validate_config({**config, "dependencies": [{"alias": "nexus_interface", "mvr": "@x/coin"}]})
        with self.assertRaisesRegex(ConfigError, "MVR"):
            validate_config({**config, "dependencies": [{"alias": "coin", "mvr": "../coin"}]})

    def test_refuses_existing_output_without_touching_it(self):
        config = self.load_config("sui")
        with tempfile.TemporaryDirectory() as parent:
            destination = Path(parent) / "existing"
            destination.mkdir()
            sentinel = destination / "keep.txt"
            sentinel.write_text("keep", encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "refusing to overwrite"):
                generate(config, destination)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_refuses_parent_traversal_and_symlink_destinations(self):
        config = self.load_config("sui")
        with tempfile.TemporaryDirectory() as parent:
            with self.assertRaisesRegex(ConfigError, "parent traversal"):
                generate(config, Path(parent) / ".." / "escaped")
            link = Path(parent) / "link"
            target = Path(parent) / "target"
            target.mkdir()
            link.symlink_to(target, target_is_directory=True)
            with self.assertRaisesRegex(ConfigError, "symbolic link"):
                generate(config, link / "generated")

    def test_generates_a_new_package_with_coin_specific_fqns(self):
        config = self.load_config("test-coin")
        with tempfile.TemporaryDirectory() as parent:
            destination = generate(config, Path(parent) / "package")
            self.assertTrue((destination / "Move.toml").is_file())
            register = (destination / "sources/register.move").read_text(encoding="utf-8")
            self.assertIn("xyz.taluslabs.agent_api.test-coin.register@2", register)
            self.assertIn("owner_public_key_hex: AsciiString", register)
            self.assertNotIn("owner_public_key: vector<u8>", register)
            self.assertNotIn("execute<", register)


if __name__ == "__main__":
    unittest.main()
