#!/usr/bin/env python3
"""Generate a concrete Nexus Agent API Move package for any configured Sui coin."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
TEMPLATE_ROOT = ROOT / "templates"
IDENTIFIER = re.compile(r"^[a-z][a-z0-9_]*$")
FQN_SEGMENT = re.compile(r"^[a-z][a-z0-9-]*$")
MOVE_ADDRESS = re.compile(r"^0x[0-9a-fA-F]+$")
MOVE_IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
MVR_PACKAGE = re.compile(r"^@[a-z0-9][a-z0-9-]*/[a-z0-9][a-z0-9-]*$")


class ConfigError(ValueError):
    """The supplied package configuration is unsafe or invalid."""


class _MoveTypeParser:
    """Parse a concrete Move type tag without accepting generic type variables."""

    def __init__(self, source: str) -> None:
        self.source = source
        self.position = 0

    def parse(self) -> tuple[Any, ...]:
        if not self.source:
            raise ValueError("empty type")
        value = self._type()
        self._skip_whitespace()
        if self.position != len(self.source):
            raise ValueError("trailing type characters")
        return value

    def _type(self) -> tuple[Any, ...]:
        self._skip_whitespace()
        for primitive in ("u8", "u16", "u32", "u64", "u128", "u256", "bool", "address", "signer"):
            if self._consume(primitive):
                return ("primitive", primitive)
        if self._consume("vector<"):
            element = self._type()
            self._expect(">")
            return ("vector", element)

        address = self._atom()
        self._expect("::")
        module = self._atom()
        self._expect("::")
        name = self._atom()
        if not IDENTIFIER.fullmatch(module) or not MOVE_IDENTIFIER.fullmatch(name):
            raise ValueError("invalid Move module or struct identifier")
        arguments: list[tuple[Any, ...]] = []
        if self._consume("<"):
            arguments.append(self._type())
            while self._consume(","):
                arguments.append(self._type())
            self._expect(">")
        return ("struct", address, module, name, tuple(arguments))

    def _atom(self) -> str:
        self._skip_whitespace()
        match = re.match(r"(?:0x[0-9a-fA-F]+|[A-Za-z_][A-Za-z0-9_]*)", self.source[self.position :])
        if match is None:
            raise ValueError("expected Move address or identifier")
        self.position += len(match.group(0))
        return match.group(0)

    def _consume(self, token: str) -> bool:
        self._skip_whitespace()
        if self.source.startswith(token, self.position):
            self.position += len(token)
            return True
        return False

    def _expect(self, token: str) -> None:
        if not self._consume(token):
            raise ValueError(f"expected {token}")

    def _skip_whitespace(self) -> None:
        while self.position < len(self.source) and self.source[self.position].isspace():
            self.position += 1


def _move_type_addresses(value: tuple[Any, ...]) -> list[str]:
    if value[0] != "struct":
        return _move_type_addresses(value[1]) if value[0] == "vector" else []
    addresses = [value[1]]
    for argument in value[4]:
        addresses.extend(_move_type_addresses(argument))
    return addresses


def validate_config(config: Any) -> dict[str, Any]:
    if not isinstance(config, dict):
        raise ConfigError("configuration must be a JSON object")
    allowed = {"package_name", "coin_slug", "coin_type", "dependencies", "named_addresses"}
    extra = set(config) - allowed
    if extra:
        raise ConfigError(f"unknown configuration keys: {', '.join(sorted(extra))}")

    package_name = config.get("package_name")
    if not isinstance(package_name, str) or not IDENTIFIER.fullmatch(package_name):
        raise ConfigError("package_name must be a lowercase Move identifier")
    coin_slug = config.get("coin_slug")
    if not isinstance(coin_slug, str) or not FQN_SEGMENT.fullmatch(coin_slug):
        raise ConfigError("coin_slug must be a lowercase FQN segment")
    coin_type = config.get("coin_type")
    try:
        coin_type_tree = _MoveTypeParser(coin_type).parse() if isinstance(coin_type, str) else None
    except ValueError as error:
        raise ConfigError("coin_type must be a concrete Move struct type") from error
    if coin_type_tree is None or coin_type_tree[0] != "struct":
        raise ConfigError("coin_type must be a concrete Move struct type")

    dependencies = config.get("dependencies", [])
    if not isinstance(dependencies, list):
        raise ConfigError("dependencies must be a list")
    aliases: set[str] = {"nexus_primitives", "nexus_interface"}
    clean_dependencies: list[dict[str, str]] = []
    for dependency in dependencies:
        if not isinstance(dependency, dict) or set(dependency) != {"alias", "mvr"}:
            raise ConfigError("each dependency must contain exactly alias and mvr")
        alias = dependency["alias"]
        package = dependency["mvr"]
        if not isinstance(alias, str) or not IDENTIFIER.fullmatch(alias):
            raise ConfigError("dependency alias must be a lowercase Move identifier")
        if alias in aliases:
            raise ConfigError(f"duplicate or reserved dependency alias: {alias}")
        if not isinstance(package, str) or not MVR_PACKAGE.fullmatch(package):
            raise ConfigError("dependencies must use an MVR package coordinate")
        aliases.add(alias)
        clean_dependencies.append({"alias": alias, "mvr": package})

    named_addresses = config.get("named_addresses", {})
    if not isinstance(named_addresses, dict):
        raise ConfigError("named_addresses must be a JSON object")
    clean_addresses: dict[str, str] = {}
    for alias, address in named_addresses.items():
        if not isinstance(alias, str) or not IDENTIFIER.fullmatch(alias):
            raise ConfigError("named address aliases must be lowercase Move identifiers")
        if alias == package_name or alias in aliases:
            raise ConfigError(f"named address alias is already defined: {alias}")
        if not isinstance(address, str) or not MOVE_ADDRESS.fullmatch(address):
            raise ConfigError(f"named address {alias} must have a hexadecimal address")
        clean_addresses[alias] = address

    available_addresses = {package_name, *clean_addresses}
    for type_address in _move_type_addresses(coin_type_tree):
        if type_address not in available_addresses and not MOVE_ADDRESS.fullmatch(type_address):
            raise ConfigError(
                f"coin type address {type_address!r} must be the package address, "
                "a configured named address, or a hexadecimal address"
            )

    return {
        "package_name": package_name,
        "coin_slug": coin_slug,
        "coin_type": coin_type,
        "dependencies": clean_dependencies,
        "named_addresses": clean_addresses,
    }


def _move_dependencies(config: dict[str, Any]) -> str:
    lines = [
        'nexus_primitives = { r.mvr = "@talus/nexus-primitives" }',
        'nexus_interface = { r.mvr = "@talus/nexus-interface" }',
    ]
    lines.extend(
        f'{item["alias"]} = {{ r.mvr = "{item["mvr"]}" }}'
        for item in config["dependencies"]
    )
    return "\n".join(lines)


def _move_addresses(config: dict[str, Any]) -> str:
    lines = [f'{config["package_name"]} = "0x0"']
    lines.extend(f'{alias} = "{address}"' for alias, address in sorted(config["named_addresses"].items()))
    return "\n".join(lines)


def render_package(config: Any) -> dict[str, str]:
    values = validate_config(config)
    substitutions = {
        "PACKAGE_NAME": values["package_name"],
        "PACKAGE_ADDRESS": values["package_name"],
        "PACKAGE_OTW": values["package_name"].upper(),
        "COIN_SLUG": values["coin_slug"],
        "COIN_TYPE": values["coin_type"],
        "MOVE_DEPENDENCIES": _move_dependencies(values),
        "MOVE_ADDRESSES": _move_addresses(values),
    }
    rendered: dict[str, str] = {}
    for template in sorted(TEMPLATE_ROOT.rglob("*.in")):
        relative = template.relative_to(TEMPLATE_ROOT)
        target = relative.with_suffix("")
        source = template.read_text(encoding="utf-8")
        for name, replacement in substitutions.items():
            source = source.replace("{{" + name + "}}", replacement)
        if "{{" in source or "}}" in source:
            raise ConfigError(f"unresolved template token in {relative}")
        rendered[target.as_posix()] = source
    if not rendered:
        raise ConfigError(f"no templates found under {TEMPLATE_ROOT}")
    return rendered


def _safe_new_directory(output: Path) -> Path:
    if ".." in output.parts:
        raise ConfigError("output path cannot contain parent traversal")
    candidate = output if output.is_absolute() else Path.cwd() / output
    for ancestor in (candidate, *candidate.parents):
        if ancestor.is_symlink():
            raise ConfigError("output path cannot cross a symbolic link")
    parent = candidate.parent.resolve(strict=True)
    if not parent.is_dir():
        raise ConfigError("output parent must be an existing directory")
    destination = parent / candidate.name
    if destination.exists() or destination.is_symlink():
        raise ConfigError(f"refusing to overwrite existing path: {destination}")
    return destination


def generate(config: Any, output: Path) -> Path:
    rendered = render_package(config)
    destination = _safe_new_directory(output)
    destination.mkdir(mode=0o755)
    try:
        for relative, contents in rendered.items():
            target = destination / relative
            if not target.resolve().is_relative_to(destination):
                raise ConfigError("generated path escaped its package directory")
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(contents, encoding="utf-8", newline="\n")
    except Exception:
        for path in sorted(destination.rglob("*"), reverse=True):
            if path.is_file() or path.is_symlink():
                path.unlink()
            elif path.is_dir():
                path.rmdir()
        destination.rmdir()
        raise
    return destination


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path, help="JSON coin/package configuration")
    parser.add_argument("--output", required=True, type=Path, help="new package directory; must not exist")
    args = parser.parse_args(argv)
    try:
        config = json.loads(args.config.read_text(encoding="utf-8"))
        destination = generate(config, args.output)
    except (OSError, json.JSONDecodeError, ConfigError) as error:
        print(f"agent-api generator: {error}", file=sys.stderr)
        return 2
    print(destination)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
