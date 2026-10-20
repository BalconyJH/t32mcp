# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Opt-in, checkpointed inspection of a connected TRACE32 target.

Run a bounded phase with an explicitly selected session port (PowerShell):
    python hardware_inspection.py --phase queries --port $env:T32_PORT --output queries.json

Memory requires an explicit address and length. Reading MMIO can have side
effects; --read-only is not an expression or memory sandbox. This probe never
issues explicit halt/reset/resume/write/load/PRACTICE commands. Before/after
state samples do not exclude transient target-state changes.
Client timeouts do not cancel a native debugger operation already in progress.
"""

import argparse
import asyncio
import json
import logging
import os
import sys
import tempfile
from datetime import UTC, datetime
from pathlib import Path

from fastmcp import Client
from fastmcp.client.transports import StdioTransport
from hardware_smoke import FORBIDDEN_TOOLS, api_port, response_object

LOGGER = logging.getLogger(__name__)

WINDOWS = ("break_list", "register", "frame", "var_local", "task_list")
CALL_TIMEOUT_SECONDS = 10


def bounded_integer(minimum, maximum):
    def parse(value):
        number = int(value, 10)
        if not minimum <= number <= maximum:
            raise argparse.ArgumentTypeError(f"must be between {minimum} and {maximum}")
        return number

    return parse


def memory_address(value):
    try:
        number = int(value, 16 if value.lower().startswith("0x") else 10)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "use decimal or 0x-prefixed hexadecimal"
        ) from error
    if not 0 <= number <= 0xFFFFFFFF:
        raise argparse.ArgumentTypeError("address must fit in 32 bits")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--phase", choices=("queries", "memory", "windows"), required=True
    )
    parser.add_argument("--port", type=api_port, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--server", default=os.environ.get("T32MCP") or "t32mcp")
    parser.add_argument(
        "--symbol", action="append", default=[], help="Exact symbol, queries only"
    )
    parser.add_argument(
        "--skills", type=Path, help="Skill directory; queries also test discovery"
    )
    parser.add_argument("--address", type=memory_address)
    parser.add_argument("--length", type=bounded_integer(1, 4096))
    parser.add_argument(
        "--access", choices=("data", "program", "arm_dap"), default="data"
    )
    parser.add_argument("--dual-port", action="store_true")
    parser.add_argument(
        "--compare",
        action="store_true",
        help="Repeat memory read with matching/mismatching expected bytes",
    )
    parser.add_argument("--window", choices=WINDOWS, action="append")
    parser.add_argument("--max-pages", type=bounded_integer(1, 8), default=2)
    parser.add_argument("--page-bytes", type=bounded_integer(1, 8192), default=128)
    args = parser.parse_args(argv)
    if args.phase == "memory":
        if args.address is None or args.length is None:
            parser.error("memory requires explicit --address and --length")
        if args.address + args.length - 1 > 0xFFFFFFFF:
            parser.error("memory address range must not wrap past 32 bits")
    elif (
        args.address is not None
        or args.length is not None
        or args.dual_port
        or args.compare
    ):
        parser.error(
            "--address, --length, --dual-port and --compare require --phase memory"
        )
    if args.symbol and args.phase != "queries":
        parser.error("--symbol requires --phase queries")
    if len(args.symbol) > 64:
        parser.error("at most 64 --symbol arguments are allowed")
    if args.window and args.phase != "windows":
        parser.error("--window requires --phase windows")
    args.window = list(dict.fromkeys(args.window or ["break_list"]))
    return args


def now():
    return datetime.now(UTC).isoformat()


def checkpoint(output, report):
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=output.parent,
            prefix=f".{output.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary = Path(stream.name)
            json.dump(report, stream, indent=2, ensure_ascii=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


class ToolFailure(Exception):
    pass


class Probe:
    def __init__(self, args):
        self.args = args
        self.client = None
        self.tools = set()
        self.report = {
            "started_utc": now(),
            "server": args.server,
            "port": args.port,
            "phase": args.phase,
            "read_only": True,
            "status": "incomplete",
            "passed": False,
            "before": None,
            "after": None,
            "state_unchanged": None,
            "operations": [],
            "phase_complete": False,
            "checks": {
                name: {"status": "pending"}
                for name in ["read_only_tools", "target_state"]
            },
        }
        self.save()

    def save(self):
        checkpoint(self.args.output, self.report)

    def check(self, name, status, detail=None):
        self.report["checks"][name] = {"status": status}
        if detail is not None:
            self.report["checks"][name]["detail"] = detail
        self.save()

    async def invoke(self, label, callback, tool=None, arguments=None):
        entry = {"label": label, "started_utc": now(), "status": "pending"}
        if tool is not None:
            entry.update(tool=tool, arguments=arguments)
        self.report["operations"].append(entry)
        self.save()
        try:
            async with asyncio.timeout(CALL_TIMEOUT_SECONDS):
                value = await callback()
            entry.update(status="ok", result=value)
            return value, "pass"
        except Exception as error:
            LOGGER.exception("Hardware observation %s failed", label)
            status = (
                "fail"
                if isinstance(error, ToolFailure) and tool != "get_debugger_status"
                else "incomplete"
            )
            entry.update(status=status, error=f"{type(error).__name__}: {error}")
            return None, status
        finally:
            entry["completed_utc"] = now()
            self.save()

    async def call(self, tool, arguments, label=None):
        label = label or tool
        if tool not in self.tools:
            self.report["operations"].append(
                {
                    "label": label,
                    "tool": tool,
                    "arguments": arguments,
                    "started_utc": now(),
                    "completed_utc": now(),
                    "status": "unsupported",
                    "error": "Tool not exposed by server",
                }
            )
            self.save()
            return None, "unsupported"

        async def request():
            result = await self.client.call_tool(tool, arguments, raise_on_error=False)
            if result.is_error:
                text = "\n".join(
                    item.text
                    for item in result.content
                    if getattr(item, "type", "") == "text"
                )
                raise ToolFailure(text or "Tool returned an error")
            try:
                return response_object(result)
            except (TypeError, ValueError) as error:
                raise ToolFailure(f"Invalid tool response: {error}") from error

        return await self.invoke(label, request, tool, arguments)

    async def check_call(self, label, tool, arguments, predicate):
        value, status = await self.call(tool, arguments, label)
        if status == "pass":
            try:
                valid = predicate(value)
            except (AttributeError, KeyError, IndexError, TypeError, ValueError):
                valid = False
            status = "pass" if valid else "fail"
        self.check(label, status)
        return value if status == "pass" else None


def state_value(status):
    state = (status or {}).get("state", {})
    if not isinstance(state, dict):
        return None
    value = state.get("value")
    return value if state.get("ok") is True and type(value) is int else None


def valid_batch(value, count):
    return (
        value.get("count") == count
        and value.get("succeeded") == count
        and value.get("failed") == 0
        and len(value.get("results", [])) == count
        and all(item.get("ok") is True for item in value["results"])
    )


async def query_phase(probe):
    batch = await probe.check_call(
        "batch_queries",
        "evaluate_expressions",
        {"expressions": ["STATE.RUN()", "PRACTICE.StackDepth()", "1.+1."]},
        lambda value: valid_batch(value, 3) and value["results"][2]["value"] == "2.",
    )
    if batch is None:
        return
    for label, expression, expected, matches in [
        ("wait_match", "STATE.RUN()", batch["results"][0]["value"], True),
        ("wait_timeout", "1.", "not-a-number", False),
    ]:
        result = await probe.check_call(
            label,
            "wait_for_condition",
            {
                "expression": expression,
                "expected_value": expected,
                "timeout_ms": 500 if matches else 130,
                "interval_ms": 40,
            },
            lambda value, matches=matches, expected=expected: (
                value.get("matched") is matches
                and value.get("timed_out") is (not matches)
                and value.get("expected_value") == expected
                and value.get("attempts", 0) >= (1 if matches else 2)
            ),
        )
        if result is None:
            return
    if probe.args.symbol:
        await probe.check_call(
            "symbol_lookup",
            "lookup_symbols",
            {"names": probe.args.symbol},
            lambda value: valid_batch(value, len(probe.args.symbol)),
        )
    if probe.args.skills:
        await probe.check_call(
            "skill_discovery",
            "list_practice_skills",
            {},
            lambda value: (
                isinstance(value.get("skills"), list) and bool(value["skills"])
            ),
        )


async def memory_phase(probe, initial_state):
    args = probe.args
    running_edap = initial_state == 3 and args.access == "arm_dap" and args.dual_port
    if initial_state not in (1, 2) and not running_edap:
        probe.check(
            "memory_read",
            "unsupported",
            "Running reads require explicit ARM DAP dual-port (EDAP); other reads require a stopped target",
        )
        return
    region = {
        "address": hex(args.address),
        "length": args.length,
        "access": args.access,
        "dual_port": args.dual_port,
    }

    def valid_memory(value):
        return valid_batch(value, 1) and (
            len(bytes.fromhex(value["results"][0]["hex"])) == args.length
        )

    memory = await probe.check_call(
        "memory_read",
        "read_memory",
        {"regions": [region]},
        valid_memory,
    )
    if not args.compare or memory is None:
        return
    expected = bytes.fromhex(memory["results"][0]["hex"]).hex()
    wrong = bytes([bytes.fromhex(expected)[0] ^ 0xFF]).hex() + expected[2:]
    for label, expected_hex, matches in [
        ("expected_bytes_match", expected, True),
        ("expected_bytes_mismatch", wrong, False),
    ]:
        result = await probe.check_call(
            label,
            "read_memory",
            {"regions": [dict(region, expected_hex=expected_hex)]},
            lambda value, matches=matches: (
                valid_memory(value)
                and value["results"][0].get("matches") is matches
                and value["results"][0].get("mismatch_count") == (0 if matches else 1)
                and value["results"][0].get("mismatch_offsets")
                == ([] if matches else [0])
            ),
        )
        if result is None:
            return


async def window_phase(probe):
    for window in probe.args.window:
        offset = 0
        for page_number in range(probe.args.max_pages):
            page, status = await probe.call(
                "read_debugger_window",
                {
                    "window": window,
                    "offset": offset,
                    "max_bytes": probe.args.page_bytes,
                },
                f"window_{window}_page_{page_number + 1}",
            )
            if status != "pass":
                probe.check("window_" + window, status)
                return
            count = page.get("bytes_read")
            valid = (
                page.get("window") == window
                and page.get("offset") == offset
                and isinstance(page.get("text"), str)
                and type(count) is int
                and 0 <= count <= probe.args.page_bytes
                and page.get("may_have_more") is (count > 0)
                and page.get("next_offset") == (offset + count if count else None)
            )
            if not valid:
                probe.check("window_" + window, "fail", "Invalid byte pagination")
                return
            probe.check(
                "window_" + window,
                "pass",
                {
                    "pages": page_number + 1,
                    "more_available": count > 0,
                },
            )
            if not count:
                break
            offset += count


async def run(args, client_factory=None):
    probe = Probe(args)
    context = None
    connected = False
    try:
        transport_args = [
            "--execution-policy", "scripts", "--native-tools",
            "--read-only", "--port", str(args.port),
        ]
        if args.skills:
            transport_args += ["--skills", str(args.skills)]

        async def connect():
            nonlocal context, connected
            transport = StdioTransport(
                command=args.server.strip("\"'"), args=transport_args
            )
            context = (client_factory or Client)(transport)
            probe.client = await context.__aenter__()
            connected = True
            return {"connected": True}

        _, status = await probe.invoke("connect", connect)
        if status == "pass":

            async def list_tools():
                return {
                    "tools": [tool.name for tool in await probe.client.list_tools()]
                }

            listing, status = await probe.invoke("list_tools", list_tools)
            if status == "pass":
                probe.tools = set(listing["tools"])
                exposed = sorted(probe.tools & FORBIDDEN_TOOLS)
                probe.check("read_only_tools", "fail" if exposed else "pass", exposed)
                if not exposed:
                    try:
                        before, _ = await probe.call(
                            "get_debugger_status", {}, "before"
                        )
                        probe.report["before"] = before
                        probe.save()
                        initial_state = state_value(before)
                        if initial_state is not None:
                            if args.phase == "queries":
                                await query_phase(probe)
                            elif args.phase == "memory":
                                await memory_phase(probe, initial_state)
                            else:
                                await window_phase(probe)
                            probe.report["phase_complete"] = True
                    finally:
                        after, _ = await probe.call("get_debugger_status", {}, "after")
                        probe.report["after"] = after
                        probe.save()
    except Exception as error:
        LOGGER.exception("Hardware inspection session failed")
        probe.report["error"] = f"{type(error).__name__}: {error}"
    finally:
        if connected:

            async def disconnect():
                await context.__aexit__(None, None, None)
                return {"disconnected": True}

            await probe.invoke("disconnect", disconnect)
        before = state_value(probe.report["before"])
        after = state_value(probe.report["after"])
        unchanged = (
            (before == after) if before is not None and after is not None else None
        )
        probe.report["state_unchanged"] = unchanged
        probe.check(
            "target_state",
            "incomplete" if unchanged is None else ("pass" if unchanged else "fail"),
        )
        statuses = [item["status"] for item in probe.report["checks"].values()]
        statuses += [item["status"] for item in probe.report["operations"]]
        if "fail" in statuses:
            outcome = "fail"
        elif "error" in probe.report or any(
            s in statuses for s in ("pending", "incomplete")
        ):
            outcome = "incomplete"
        elif "unsupported" in statuses:
            outcome = "unsupported"
        elif probe.report["phase_complete"] and len(probe.report["checks"]) > 2:
            outcome = "pass"
        else:
            outcome = "incomplete"
        probe.report.update(
            status=outcome, passed=outcome == "pass", completed_utc=now()
        )
        probe.save()
    return probe.report


def main(argv=None):
    args = parse_args(argv)
    report = asyncio.run(run(args))
    print(json.dumps(report, indent=2, ensure_ascii=True))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
