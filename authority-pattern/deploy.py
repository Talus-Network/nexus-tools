#!/usr/bin/env python3
"""Validate and print a non-executing Agent API deployment plan."""

from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
import tomllib
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from onchain.agent_api.generate import _MoveTypeParser


OPERATIONS = ("register", "charge", "authorize", "revoke")
ONCHAIN_FQN_VERSIONS = {
    "register": "2",
    "charge": "1",
    "authorize": "1",
    "revoke": "1",
}
BUNDLE_ROOT = Path(__file__).resolve().parent
ONCHAIN_DESCRIPTIONS = {
    "register": "Register an Agent API binding.",
    "charge": "Charge an Agent API binding.",
    "authorize": "Authorize one Agent API request.",
    "revoke": "Revoke an Agent API binding.",
}
ADDRESS = re.compile(r"^0x[0-9a-fA-F]{1,64}$")
IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
SLUG = re.compile(r"^[a-z][a-z0-9-]*$")
ENV_NAME = re.compile(r"^[A-Z_][A-Z0-9_]*$")
DECIMAL = re.compile(r"^(0|[1-9][0-9]*)$")
MAX_U64 = (1 << 64) - 1


class ConfigError(ValueError):
    """A deployment configuration is incomplete or malformed."""


def _unknown_keys(value: dict[str, Any], allowed: set[str], label: str) -> list[str]:
    return [f"{label}.{key}: unsupported field" for key in sorted(set(value) - allowed)]


def _text(value: Any, label: str, errors: list[str]) -> str:
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{label}: required non-empty string")
        return ""
    return value.strip()


def _object_id(value: Any, label: str, errors: list[str], required: bool) -> str:
    if value == "" and not required:
        return ""
    text = _text(value, label, errors)
    if text and not ADDRESS.fullmatch(text):
        errors.append(f"{label}: expected a Sui object ID such as 0x followed by hex digits")
    return text


def _mist(value: Any, label: str, errors: list[str]) -> str:
    text = _text(value, label, errors)
    if text and not DECIMAL.fullmatch(text):
        errors.append(f"{label}: expected a non-negative integer in MIST")
    return text


def _credit_rate(value: Any, label: str, errors: list[str]) -> str:
    if value == "":
        return ""
    text = _text(value, label, errors)
    if not text:
        return ""
    if not DECIMAL.fullmatch(text):
        errors.append(f"{label}: expected a positive decimal u64")
    elif len(text) > 20 or not 0 < int(text) <= MAX_U64:
        errors.append(f"{label}: expected a positive decimal u64")
    return text


def _url(value: Any, label: str, errors: list[str], https_only: bool = False) -> str:
    text = _text(value, label, errors)
    if not text:
        return ""
    try:
        parsed = urlsplit(text)
        hostname = parsed.hostname
        parsed.port
    except ValueError:
        errors.append(f"{label}: invalid URL")
        return text
    allowed_schemes = {"https"} if https_only else {"http", "https"}
    if parsed.scheme not in allowed_schemes or not hostname:
        errors.append(f"{label}: expected an absolute {'HTTPS' if https_only else 'HTTP(S)'} URL")
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        errors.append(f"{label}: credentials, query parameters, and fragments belong in secret configuration")
    return text


def _env_name(value: Any, label: str, errors: list[str]) -> str:
    text = _text(value, label, errors)
    if text and not ENV_NAME.fullmatch(text):
        errors.append(f"{label}: expected an environment variable name, never a secret value")
    return text


def _runtime_env_name(value: Any, label: str, runtime_name: str, errors: list[str]) -> str:
    text = _env_name(value, label, errors)
    if text and text != runtime_name:
        errors.append(f"{label}: runtime reads the fixed variable {runtime_name}")
    return text


def _resolve_path(value: Any, label: str, config_dir: Path, errors: list[str]) -> Path | None:
    text = _text(value, label, errors)
    if not text:
        return None
    path = Path(text)
    if not path.is_absolute():
        path = config_dir / path
    return path.resolve()


def validate_config(
    value: Any,
    config_dir: Path,
    *,
    require_registered: bool,
) -> tuple[dict[str, Any], list[str], list[str]]:
    """Return normalized config, fatal errors, and post-publication inputs."""
    errors: list[str] = []
    pending: list[str] = []
    if not isinstance(value, dict):
        return {}, ["configuration: expected a JSON object"], []
    errors.extend(_unknown_keys(value, {"schema_version", "coins", "service", "worker"}, "config"))
    if value.get("schema_version") != 1:
        errors.append("schema_version: expected 1")

    raw_coins = value.get("coins")
    if not isinstance(raw_coins, list) or not raw_coins:
        errors.append("coins: expected a non-empty array")
        raw_coins = []
    coins: list[dict[str, Any]] = []
    seen_slugs: set[str] = set()
    seen_cashiers: set[str] = set()
    seen_operator_caps: set[str] = set()
    seen_caps: set[str] = set()
    seen_event_streams: set[tuple[str, str]] = set()
    coin_allowed = {
        "coin_slug",
        "package_name",
        "package_path",
        "coin_type",
        "event_rpc_url",
        "invocation_cost_mist",
        "package_id",
        "cashier_id",
        "operator_cap_id",
        "credit_rate",
        "settlement_cap_id",
        "tool_witness_ids",
        "collateral_coin_id",
    }
    for index, raw_coin in enumerate(raw_coins):
        label = f"coins[{index}]"
        if not isinstance(raw_coin, dict):
            errors.append(f"{label}: expected a JSON object")
            continue
        errors.extend(_unknown_keys(raw_coin, coin_allowed, label))
        slug = _text(raw_coin.get("coin_slug"), f"{label}.coin_slug", errors)
        if slug and not SLUG.fullmatch(slug):
            errors.append(f"{label}.coin_slug: use lowercase letters, digits, and hyphens")
        if slug in seen_slugs:
            errors.append(f"{label}.coin_slug: duplicate FQN namespace")
        seen_slugs.add(slug)

        package_name = _text(raw_coin.get("package_name"), f"{label}.package_name", errors)
        if package_name and not IDENTIFIER.fullmatch(package_name):
            errors.append(f"{label}.package_name: expected a Move identifier")
        package_path = _resolve_path(
            raw_coin.get("package_path"), f"{label}.package_path", config_dir, errors
        )
        if package_path and not (package_path.is_dir() and (package_path / "Move.toml").is_file()):
            errors.append(f"{label}.package_path: expected an existing generated Move package")
        elif package_path:
            try:
                move_manifest = tomllib.loads((package_path / "Move.toml").read_text(encoding="utf-8"))
                manifest_name = move_manifest["package"]["name"]
            except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError):
                errors.append(f"{label}.package_path: could not read the generated Move package name")
            else:
                if manifest_name != package_name:
                    errors.append(f"{label}.package_name: does not match package_path/Move.toml")
        coin_type = _text(raw_coin.get("coin_type"), f"{label}.coin_type", errors)
        if coin_type:
            try:
                parsed_type = _MoveTypeParser(coin_type).parse()
            except ValueError:
                errors.append(f"{label}.coin_type: expected a concrete Move type")
            else:
                if parsed_type[0] != "struct":
                    errors.append(f"{label}.coin_type: expected a concrete struct type")
        event_rpc_url = _url(raw_coin.get("event_rpc_url"), f"{label}.event_rpc_url", errors)
        cost = _mist(raw_coin.get("invocation_cost_mist"), f"{label}.invocation_cost_mist", errors)

        package_id = _object_id(raw_coin.get("package_id", ""), f"{label}.package_id", errors, False)
        cashier_id = _object_id(raw_coin.get("cashier_id", ""), f"{label}.cashier_id", errors, False)
        operator_cap_id = _object_id(
            raw_coin.get("operator_cap_id", ""), f"{label}.operator_cap_id", errors, False
        )
        credit_rate = _credit_rate(
            raw_coin.get("credit_rate", ""), f"{label}.credit_rate", errors
        )
        cap_id = _object_id(
            raw_coin.get("settlement_cap_id", ""), f"{label}.settlement_cap_id", errors, False
        )
        collateral_id = _object_id(
            raw_coin.get("collateral_coin_id", ""), f"{label}.collateral_coin_id", errors, False
        )
        witnesses = raw_coin.get("tool_witness_ids", {})
        if not isinstance(witnesses, dict):
            errors.append(f"{label}.tool_witness_ids: expected an object")
            witnesses = {}
        errors.extend(_unknown_keys(witnesses, set(OPERATIONS), f"{label}.tool_witness_ids"))
        normalized_witnesses = {
            operation: _object_id(
                witnesses.get(operation, ""),
                f"{label}.tool_witness_ids.{operation}",
                errors,
                False,
            )
            for operation in OPERATIONS
        }
        if package_id:
            required_values = {
                "cashier_id": cashier_id,
                "operator_cap_id": operator_cap_id,
                "credit_rate": credit_rate,
                "settlement_cap_id": cap_id,
                **{f"tool_witness_ids.{op}": normalized_witnesses[op] for op in OPERATIONS},
            }
            missing = [name for name, item in required_values.items() if not item]
            if missing:
                pending.extend(f"coins[{slug}].{name}" for name in missing)
            if cashier_id and cap_id:
                if cashier_id in seen_cashiers:
                    errors.append(f"{label}.cashier_id: duplicate cashier route")
                if cap_id in seen_caps:
                    errors.append(f"{label}.settlement_cap_id: duplicate settlement capability")
                seen_cashiers.add(cashier_id)
                seen_caps.add(cap_id)
                stream = (package_id, "accounting")
                if stream in seen_event_streams:
                    errors.append(f"{label}: duplicate package/accounting event stream")
                seen_event_streams.add(stream)
        else:
            pending.extend(
                [
                    f"coins[{slug}].package_id",
                    f"coins[{slug}].cashier_id",
                    f"coins[{slug}].operator_cap_id",
                    f"coins[{slug}].credit_rate",
                    f"coins[{slug}].settlement_cap_id",
                    *[f"coins[{slug}].tool_witness_ids.{op}" for op in OPERATIONS],
                ]
            )
        if operator_cap_id:
            if operator_cap_id in seen_operator_caps:
                errors.append(f"{label}.operator_cap_id: duplicate operator capability")
            seen_operator_caps.add(operator_cap_id)
        coins.append(
            {
                "coin_slug": slug,
                "package_name": package_name,
                "package_path": package_path,
                "coin_type": coin_type,
                "event_rpc_url": event_rpc_url,
                "invocation_cost_mist": cost,
                "package_id": package_id,
                "cashier_id": cashier_id,
                "operator_cap_id": operator_cap_id,
                "credit_rate": credit_rate,
                "settlement_cap_id": cap_id,
                "tool_witness_ids": normalized_witnesses,
                "collateral_coin_id": collateral_id,
            }
        )

    service_allowed = {
        "public_url",
        "provider_url",
        "bind_addr",
        "database_path",
        "master_key_env",
        "provider_operator_key_env",
        "toolkit_config_path",
        "allowed_leaders_path",
        "query_signing_key_file",
        "retrieve_key_signing_key_file",
        "http_tool_fqn_version",
        "invocation_cost_mist",
        "collateral_coin_id",
    }
    raw_service = value.get("service")
    if not isinstance(raw_service, dict):
        errors.append("service: expected a JSON object")
        raw_service = {}
    errors.extend(_unknown_keys(raw_service, service_allowed, "service"))
    service = {
        "public_url": _url(raw_service.get("public_url"), "service.public_url", errors, True),
        "provider_url": _url(raw_service.get("provider_url"), "service.provider_url", errors),
        "bind_addr": _text(raw_service.get("bind_addr"), "service.bind_addr", errors),
        "database_path": _resolve_path(
            raw_service.get("database_path"), "service.database_path", config_dir, errors
        ),
        "master_key_env": _runtime_env_name(
            raw_service.get("master_key_env"),
            "service.master_key_env",
            "AGENT_API_MASTER_KEY",
            errors,
        ),
        "provider_operator_key_env": _runtime_env_name(
            raw_service.get("provider_operator_key_env"),
            "service.provider_operator_key_env",
            "AGENT_API_PROVIDER_OPERATOR_KEY",
            errors,
        ),
        "toolkit_config_path": _resolve_path(
            raw_service.get("toolkit_config_path"), "service.toolkit_config_path", config_dir, errors
        ),
        "allowed_leaders_path": _resolve_path(
            raw_service.get("allowed_leaders_path"), "service.allowed_leaders_path", config_dir, errors
        ),
        "query_signing_key_file": _resolve_path(
            raw_service.get("query_signing_key_file"), "service.query_signing_key_file", config_dir, errors
        ),
        "retrieve_key_signing_key_file": _resolve_path(
            raw_service.get("retrieve_key_signing_key_file"),
            "service.retrieve_key_signing_key_file",
            config_dir,
            errors,
        ),
        "http_tool_fqn_version": _text(
            raw_service.get("http_tool_fqn_version"), "service.http_tool_fqn_version", errors
        ),
        "invocation_cost_mist": _mist(
            raw_service.get("invocation_cost_mist"), "service.invocation_cost_mist", errors
        ),
        "collateral_coin_id": _object_id(
            raw_service.get("collateral_coin_id", ""),
            "service.collateral_coin_id",
            errors,
            False,
        ),
    }
    if service["http_tool_fqn_version"] and not DECIMAL.fullmatch(service["http_tool_fqn_version"]):
        errors.append("service.http_tool_fqn_version: expected the numeric version used to build the binary")

    worker_allowed = {"sui_grpc_url", "settlement_signer_key_env", "settlement_gas_budget_mist"}
    raw_worker = value.get("worker")
    if not isinstance(raw_worker, dict):
        errors.append("worker: expected a JSON object")
        raw_worker = {}
    errors.extend(_unknown_keys(raw_worker, worker_allowed, "worker"))
    gas_budget = raw_worker.get("settlement_gas_budget_mist", "50000000")
    worker = {
        "sui_grpc_url": _url(raw_worker.get("sui_grpc_url"), "worker.sui_grpc_url", errors),
        "settlement_signer_key_env": _runtime_env_name(
            raw_worker.get("settlement_signer_key_env"),
            "worker.settlement_signer_key_env",
            "AGENT_API_SETTLEMENT_SIGNER_KEY_B64",
            errors,
        ),
        "settlement_gas_budget_mist": _mist(
            gas_budget, "worker.settlement_gas_budget_mist", errors
        ),
    }

    if require_registered and pending:
        errors.extend(f"required after publication: {item}" for item in pending)
    normalized = {"coins": coins, "service": service, "worker": worker}
    return normalized, errors, pending


def _command(parts: list[str]) -> str:
    return shlex.join(parts)


def _agent_api_run_command(*arguments: str) -> str:
    return _command(
        [
            "cargo",
            "run",
            "--locked",
            "--release",
            "--manifest-path",
            str(BUNDLE_ROOT / "offchain" / "Cargo.toml"),
            "--bin",
            "agent-api",
            *arguments,
        ]
    )


def _fqn(slug: str, operation: str) -> str:
    return f"xyz.taluslabs.agent_api.{slug}.{operation}@{ONCHAIN_FQN_VERSIONS[operation]}"


def _http_fqn(tool: str, version: str) -> str:
    return f"xyz.taluslabs.agent_api.{tool}@{version}"


def _route_records(config: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "rpc_url": coin["event_rpc_url"],
            "package_id": coin["package_id"],
            "module": "accounting",
            "coin_type": coin["coin_type"],
            "cashier_id": coin["cashier_id"],
            "settlement_cap_id": coin["settlement_cap_id"],
        }
        for coin in config["coins"]
        if coin["package_id"] and coin["cashier_id"] and coin["settlement_cap_id"]
    ]


def render_plan(config: dict[str, Any]) -> str:
    service = config["service"]
    worker = config["worker"]
    mock_provider_db = service["database_path"].with_name("agent-api-mock-provider.sqlite")
    lines = [
        "Agent API deployment preparation plan",
        "Execution: NOT EXECUTED. This command only validates input and prints operator instructions.",
        "Review the selected Sui/Nexus environment, signer, gas, and collateral before running any listed mutation.",
        "",
    ]
    for coin in config["coins"]:
        slug = coin["coin_slug"]
        lines.append(f"[{slug}] Move package")
        if not coin["package_id"]:
            lines.append("NOT EXECUTED: " + _command(["sui", "client", "publish", str(coin["package_path"])]))
            lines.append(
                "After publication, record package_id, cashier_id, operator_cap_id, settlement_cap_id, and the four witness IDs; set a positive credit_rate in the configuration."
            )
        elif not coin["cashier_id"] or not coin["operator_cap_id"] or not coin["credit_rate"]:
            lines.append(
                "Cashier pricing setup and onchain Tool registration deferred until cashier_id, operator_cap_id, and a positive credit_rate are configured."
            )
            for name, item in (
                ("cashier_id", coin["cashier_id"]),
                ("operator_cap_id", coin["operator_cap_id"]),
                ("credit_rate", coin["credit_rate"]),
            ):
                if not item:
                    lines.append(f"Required input: coins[{slug}].{name}")
        else:
            lines.append("[operator cashier pricing]")
            lines.append("Set or update the cashier rate before registering its onchain Tools.")
            lines.append(
                "NOT EXECUTED: "
                + _command(
                    [
                        "sui",
                        "client",
                        "ptb",
                        "--move-call",
                        f"{coin['package_id']}::accounting::set_credit_rate",
                        f"<{coin['coin_type']}>",
                        f"@{coin['operator_cap_id']}",
                        f"@{coin['cashier_id']}",
                        coin["credit_rate"],
                    ]
                )
            )
            if not coin["settlement_cap_id"] or any(
                not coin["tool_witness_ids"][operation] for operation in OPERATIONS
            ):
                lines.append("Onchain Tool registration deferred; settlement-cap or Tool-witness IDs are missing.")
                for name, item in (
                    ("settlement_cap_id", coin["settlement_cap_id"]),
                    *[
                        (f"tool_witness_ids.{operation}", coin["tool_witness_ids"][operation])
                        for operation in OPERATIONS
                    ],
                ):
                    if not item:
                        lines.append(f"Required input: coins[{slug}].{name}")
            else:
                for operation in OPERATIONS:
                    args = [
                        "nexus",
                        "tool",
                        "register",
                        "onchain",
                        "--package",
                        coin["package_id"],
                        "--module",
                        operation,
                        "--tool-fqn",
                        _fqn(slug, operation),
                        "--description",
                        ONCHAIN_DESCRIPTIONS[operation],
                        "--tool-witness-id",
                        coin["tool_witness_ids"][operation],
                        "--invocation-cost",
                        coin["invocation_cost_mist"],
                    ]
                    if coin["collateral_coin_id"]:
                        args.extend(["--collateral-coin", coin["collateral_coin_id"]])
                    lines.append("NOT EXECUTED: " + _command(args))
        lines.append("")

    query_fqn = _http_fqn("query", service["http_tool_fqn_version"])
    retrieve_fqn = _http_fqn("retrieve-key", service["http_tool_fqn_version"])
    lines.extend(
        [
            "[shared signed HTTP Tools]",
            f"FQNs served at {service['public_url']}: {query_fqn}, {retrieve_fqn}",
            "NOT EXECUTED: "
            + _command(
                [
                    "nexus",
                    "tool",
                    "register",
                    "offchain",
                    "--url",
                    service["public_url"],
                    "--batch",
                    "--invocation-cost",
                    service["invocation_cost_mist"],
                ]
            ),
        ]
    )
    if service["collateral_coin_id"]:
        lines[-1] += " --collateral-coin " + shlex.quote(service["collateral_coin_id"])
    lines.extend(
        [
            "Protect the generated key files and configure their private keys in the external Toolkit secret/config mount.",
            "NOT EXECUTED: umask 077",
            "NOT EXECUTED: "
            + _command(
                ["nexus", "tool", "auth", "keygen", "--out", str(service["query_signing_key_file"])]
            ),
            "NOT EXECUTED: "
            + _command(
                [
                    "nexus",
                    "tool",
                    "auth",
                    "keygen",
                    "--out",
                    str(service["retrieve_key_signing_key_file"]),
                ]
            ),
            "NOT EXECUTED: "
            + _command(
                [
                    "nexus",
                    "tool",
                    "auth",
                    "register-key",
                    "--tool-fqn",
                    query_fqn,
                    "--signing-key",
                    str(service["query_signing_key_file"]),
                    "--skip-if-active",
                ]
            ),
            "NOT EXECUTED: "
            + _command(
                [
                    "nexus",
                    "tool",
                    "auth",
                    "register-key",
                    "--tool-fqn",
                    retrieve_fqn,
                    "--signing-key",
                    str(service["retrieve_key_signing_key_file"]),
                    "--skip-if-active",
                ]
            ),
            "NOT EXECUTED: "
            + _command(
                [
                    "nexus",
                    "tool",
                    "auth",
                    "export-allowed-leaders",
                    "--all",
                    "--out",
                    str(service["allowed_leaders_path"]),
                ]
            ),
            "",
        ]
    )

    routes = _route_records(config)
    if len(routes) != len(config["coins"]):
        lines.append("Runtime service/worker commands deferred until every package has a cashier and settlement capability.")
        lines.append("")
        lines.append("Required inputs:")
        for coin in config["coins"]:
            if not coin["package_id"]:
                lines.append(f"- coins[{coin['coin_slug']}].package_id, cashier_id, operator_cap_id, credit_rate, settlement_cap_id, and four tool_witness_ids")
            else:
                for name, item in (
                    ("cashier_id", coin["cashier_id"]),
                    ("operator_cap_id", coin["operator_cap_id"]),
                    ("credit_rate", coin["credit_rate"]),
                    ("settlement_cap_id", coin["settlement_cap_id"]),
                    *[(f"tool_witness_ids.{op}", coin["tool_witness_ids"][op]) for op in OPERATIONS],
                ):
                    if not item:
                        lines.append(f"- coins[{coin['coin_slug']}].{name}")
        return "\n".join(lines) + "\n"

    routes_json = json.dumps(routes, separators=(",", ":"))
    lines.extend(
        [
            "[service and event/refund worker]",
            f"AGENT_API_DEPLOYMENTS={shlex.quote(routes_json)}",
            f"AGENT_API_DB_PATH={shlex.quote(str(service['database_path']))}",
            f"AGENT_API_PROVIDER_URL={shlex.quote(service['provider_url'])}",
            f"BIND_ADDR={shlex.quote(service['bind_addr'])}",
            f"{service['master_key_env']}=${{{service['master_key_env']}:?load from the configured secret manager}}",
            f"{service['provider_operator_key_env']}=${{{service['provider_operator_key_env']}:?load from the configured secret manager}}",
            f"NEXUS_TOOLKIT_CONFIG_PATH={shlex.quote(str(service['toolkit_config_path']))}",
            "NOT EXECUTED: " + _agent_api_run_command(),
            "",
            "[local mock provider]",
            "Use only with synthetic local credentials; this does not configure a production provider.",
            "NOT EXECUTED: AGENT_API_MOCK_PROVIDER_BIND_ADDR=127.0.0.1:8091 \\",
            f"  AGENT_API_MOCK_OPERATOR_KEY=${{{service['provider_operator_key_env']}:?load a synthetic local key}} \\",
            f"  AGENT_API_MOCK_PROVIDER_DB={shlex.quote(str(mock_provider_db))} \\",
            "  " + _agent_api_run_command("--", "mock-provider"),
            "",
            "[settlement/refund worker]",
            f"AGENT_API_SUI_GRPC_URL={shlex.quote(worker['sui_grpc_url'])}",
            f"{worker['settlement_signer_key_env']}=${{{worker['settlement_signer_key_env']}:?load from the configured secret manager}}",
            f"AGENT_API_SETTLEMENT_GAS_BUDGET={shlex.quote(worker['settlement_gas_budget_mist'])}",
            "NOT EXECUTED: " + _agent_api_run_command("--", "worker"),
        ]
    )
    return "\n".join(lines) + "\n"


def _read_config(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ConfigError(f"could not read valid JSON configuration at {path}") from error


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for command in ("validate", "plan"):
        subparser = subparsers.add_parser(command)
        subparser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        raw = _read_config(args.config)
    except ConfigError as error:
        print(f"configuration error: {error}", file=sys.stderr)
        return 2
    normalized, errors, pending = validate_config(
        raw,
        args.config.resolve().parent,
        require_registered=args.command == "validate",
    )
    if errors:
        for error in errors:
            print(f"configuration error: {error}", file=sys.stderr)
        return 2
    if args.command == "validate":
        print("Deployment configuration is complete and valid; no commands were executed.")
        return 0
    if pending:
        print("Post-publication inputs still required:")
        for item in pending:
            print(f"- {item}")
        print()
    print(render_plan(normalized), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
