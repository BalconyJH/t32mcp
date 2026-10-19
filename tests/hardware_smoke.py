# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Explicit, read-only hardware smoke test; never collected by pytest.

PowerShell usage after setting T32_PORT to the explicitly selected session:
    uv run python hardware_smoke.py --port $env:T32_PORT
    uv run python hardware_smoke.py --port $env:T32_PORT --register REGISTER_NAME
    uv run python hardware_smoke.py --port $env:T32_PORT --symbol SYMBOL_NAME

Only debugger status is requested by default. Register and symbol names must
be explicitly selected for the target architecture and loaded program. The
script never halts, resumes, resets, writes memory/registers, evaluates arbitrary
expressions, reads MMIO, or executes a PRACTICE script.
"""

import argparse
import asyncio
import json
import logging
import os
import sys
from datetime import UTC, datetime
from pathlib import Path

from fastmcp import Client
from fastmcp.client.transports import StdioTransport

LOGGER = logging.getLogger(__name__)

FORBIDDEN_TOOLS = {
    "execute_practice",
    "execute_practice_skill",
    "abort_practice_skill",
}


def api_port(value: str) -> int:
    number = int(value)
    if not 1 <= number <= 65535:
        raise argparse.ArgumentTypeError("port must be between 1 and 65535")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=api_port, required=True)
    default_server = os.environ.get("T32MCP") or str(
        Path(__file__).resolve().parents[1]
        / "t32mcp-rs"
        / "target"
        / "debug"
        / ("t32mcp.exe" if os.name == "nt" else "t32mcp")
    )
    parser.add_argument("--server", default=default_server)
    parser.add_argument(
        "--register", action="append", default=[], help="Optional exact register name"
    )
    parser.add_argument(
        "--symbol", action="append", default=[], help="Optional exact symbol name"
    )
    parser.add_argument("--output", type=Path)
    return parser.parse_args(argv)


def response_object(result) -> dict:
    structured = getattr(result, "structured_content", None)
    if isinstance(structured, dict):
        return structured
    for item in result.content:
        if getattr(item, "type", None) == "text":
            value = json.loads(item.text)
            if isinstance(value, dict):
                return value
    raise ValueError("Tool did not return a JSON object")


async def inspect(client: Client, name: str, arguments: dict) -> dict:
    async with asyncio.timeout(20):
        result = await client.call_tool(name, arguments)
    return response_object(result)


def requested_batch_succeeded(value, names: list[str]) -> bool:
    if not names:
        return True
    if not isinstance(value, dict):
        return False
    results = value.get("results")
    return (
        value.get("count") == len(names)
        and value.get("succeeded") == len(names)
        and value.get("failed") == 0
        and isinstance(results, list)
        and len(results) == len(names)
        and all(
            isinstance(item, dict)
            and item.get("name") == name
            and item.get("ok") is True
            for name, item in zip(names, results, strict=True)
        )
    )


async def run(args, *, client_factory=None) -> dict:
    server = Path(args.server.strip('"').strip("'")).expanduser().resolve()
    if not server.is_file():
        raise FileNotFoundError(f"MCP executable not found: {server}")
    report = {
        "started_utc": datetime.now(UTC).isoformat(),
        "server": str(server),
        "port": args.port,
        "read_only": True,
        "operations": [],
        "before": None,
        "registers": None,
        "symbols": None,
        "after": None,
        "errors": [],
    }
    transport = StdioTransport(
        command=str(server),
        args=["--native-tools", "--read-only", "--port", str(args.port)],
    )
    async with (client_factory or Client)(transport) as client:
        names = {tool.name for tool in await client.list_tools()}
        exposed = names & FORBIDDEN_TOOLS
        if exposed:
            raise RuntimeError(
                f"Read-only server exposes unsafe tools: {sorted(exposed)}"
            )
        try:
            report["operations"].append("get_debugger_status")
            report["before"] = await inspect(client, "get_debugger_status", {})
            if args.register:
                report["operations"].append("read_registers")
                report["registers"] = await inspect(
                    client, "read_registers", {"names": args.register}
                )
            if args.symbol:
                report["operations"].append("lookup_symbols")
                report["symbols"] = await inspect(
                    client, "lookup_symbols", {"names": args.symbol}
                )
        except Exception as error:
            LOGGER.exception("Hardware smoke observations failed")
            report["errors"].append(f"{type(error).__name__}: {error}")
        finally:
            try:
                report["operations"].append("get_debugger_status")
                report["after"] = await inspect(client, "get_debugger_status", {})
            except Exception as error:
                LOGGER.exception("Post-observation debugger status failed")
                report["errors"].append(f"after: {type(error).__name__}: {error}")

    before = (report["before"] or {}).get("state", {})
    after = (report["after"] or {}).get("state", {})
    states_available = before.get("ok") is True and after.get("ok") is True
    report["state_unchanged"] = (
        before.get("value") == after.get("value") if states_available else None
    )
    report["passed"] = (
        not report["errors"]
        and states_available
        and report["state_unchanged"] is True
        and requested_batch_succeeded(report["registers"], args.register)
        and requested_batch_succeeded(report["symbols"], args.symbol)
    )
    report["completed_utc"] = datetime.now(UTC).isoformat()
    return report


def main() -> int:
    args = parse_args()
    try:
        report = asyncio.run(run(args))
    except Exception as error:
        LOGGER.exception("Hardware smoke session failed")
        report = {
            "port": args.port,
            "read_only": True,
            "passed": False,
            "error": f"{type(error).__name__}: {error}",
        }
    text = json.dumps(report, indent=2, ensure_ascii=True)
    print(text)
    if args.output:
        args.output.write_text(text + "\n", encoding="utf-8")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
