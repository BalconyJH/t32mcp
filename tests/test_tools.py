# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Protocol and validation regressions that must not access target hardware."""

import asyncio
import json
import time
from pathlib import Path

import pytest
from conftest import NATIVE_TOOLS, OFFLINE_NATIVE_TOOLS
from mcp.shared.exceptions import McpError


def result_text(result) -> str:
    return "\n".join(
        item.text for item in result.content if getattr(item, "type", None) == "text"
    )


def result_object(result) -> dict:
    structured = getattr(result, "structured_content", None)
    if isinstance(structured, dict):
        return structured
    for item in result.content:
        if getattr(item, "type", None) == "text":
            parsed = json.loads(item.text)
            if isinstance(parsed, dict):
                return parsed
    raise AssertionError(f"Expected a JSON object response, got {result!r}")


async def tool_error(client, name: str, arguments: dict) -> str:
    try:
        result = await client.call_tool(name, arguments, raise_on_error=False)
    except McpError as error:
        assert error.error.code in {-32601, -32602}, error
        return str(error)
    assert result.is_error, result
    return result_text(result)


@pytest.mark.parametrize(
    ("tool_name", "required", "properties"),
    [
        ("evaluate_expressions", {"expressions"}, {"expressions"}),
        ("read_registers", {"names"}, {"names"}),
        ("lookup_symbols", {"names"}, {"names"}),
        ("read_memory", {"regions"}, {"regions"}),
        ("read_debugger_window", {"window"}, {"window", "offset", "max_bytes"}),
        (
            "wait_for_condition",
            {"expression", "expected_value"},
            {"expression", "expected_value", "timeout_ms", "interval_ms"},
        ),
    ],
)
async def test_inspection_tool_schemas(
    native_mcp_server, tool_name, required, properties
):
    async with native_mcp_server as client:
        tools = {tool.name: tool for tool in await client.list_tools()}
    schema = tools[tool_name].inputSchema
    assert set(schema.get("required", [])) == required
    assert set(schema["properties"]) == properties | {"port"}
    assert schema.get("additionalProperties") is False


@pytest.mark.parametrize("tool_name", sorted(NATIVE_TOOLS))
async def test_native_tool_port_selection_schema(native_mcp_server, tool_name):
    async with native_mcp_server as client:
        tools = {tool.name: tool for tool in await client.list_tools()}
    schema = tools[tool_name].inputSchema
    if tool_name in OFFLINE_NATIVE_TOOLS:
        assert "port" not in schema.get("properties", {})
    else:
        assert "port" in schema["properties"]
        assert "port" not in schema.get("required", [])


@pytest.mark.parametrize(
    ("tool_name", "arguments", "expected"),
    [
        ("evaluate_expressions", {"expressions": []}, "between 1 and 64"),
        ("evaluate_expressions", {"expressions": ["1"] * 65}, "between 1 and 64"),
        ("evaluate_expressions", {"expressions": [""]}, "must not be empty"),
        ("evaluate_expressions", {"expressions": ["1;Go"]}, "semicolons"),
        ("evaluate_expressions", {"expressions": ["1\nGo"]}, "control characters"),
        ("evaluate_expressions", {"expressions": ["x" * 4097]}, "4096"),
        ("read_registers", {"names": []}, "between 1 and 64"),
        ("read_registers", {"names": ["PC", "SP;Go"]}, "semicolons"),
        ("lookup_symbols", {"names": []}, "between 1 and 64"),
        ("lookup_symbols", {"names": ["main", "\x00"]}, "control characters"),
        ("read_memory", {"regions": []}, "between 1 and 64"),
        (
            "read_memory",
            {"regions": [{"address": "0x0", "length": 0}]},
            "length must be between",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0x0", "length": 4097}]},
            "length must be between",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0x100000000", "length": 1}]},
            "32-bit",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0xFFFFFFFF", "length": 2}]},
            "32-bit",
        ),
        (
            "read_memory",
            {"regions": [{"address": "-1", "length": 1}]},
            "address must be",
        ),
        (
            "read_memory",
            {"regions": [{"address": "main", "length": 1}]},
            "address must be",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0", "length": 1, "expected_hex": "0000"}]},
            "exactly 1 bytes",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0", "length": 1, "expected_hex": "GG"}]},
            "non-hexadecimal",
        ),
        (
            "read_memory",
            {"regions": [{"address": "0", "length": 4096}] * 17},
            "65536",
        ),
        (
            "read_memory",
            {
                "regions": [
                    {"address": "0", "length": 1},
                    {"address": "0x100000000", "length": 1},
                ]
            },
            "32-bit",
        ),
        (
            "read_debugger_window",
            {"window": "register", "max_bytes": 0},
            "max_bytes",
        ),
        (
            "read_debugger_window",
            {"window": "register", "max_bytes": 8193},
            "max_bytes",
        ),
        (
            "read_debugger_window",
            {"window": "register", "offset": 4294967295, "max_bytes": 1},
            "32-bit",
        ),
        (
            "wait_for_condition",
            {"expression": "1;Go", "expected_value": "1"},
            "semicolons",
        ),
        (
            "wait_for_condition",
            {"expression": "1", "expected_value": "x" * 4097},
            "4096",
        ),
        (
            "wait_for_condition",
            {"expression": "1", "expected_value": "1", "timeout_ms": 0},
            "timeout_ms",
        ),
        (
            "wait_for_condition",
            {"expression": "1", "expected_value": "1", "timeout_ms": 60001},
            "timeout_ms",
        ),
        (
            "wait_for_condition",
            {"expression": "1", "expected_value": "1", "interval_ms": 19},
            "interval_ms",
        ),
        (
            "wait_for_condition",
            {"expression": "1", "expected_value": "1", "interval_ms": 60001},
            "interval_ms",
        ),
    ],
)
async def test_invalid_requests_are_rejected_before_connecting(
    native_mcp_server, tool_name, arguments, expected
):
    async with native_mcp_server as client:
        error = await tool_error(client, tool_name, arguments)
    assert expected in error, error
    assert "127.0.0.1:" not in error, error
    assert "RCL=NETTCP" not in error, error


@pytest.mark.parametrize(
    ("tool_name", "arguments", "expected"),
    [
        ("evaluate_expressions", {}, "expressions"),
        ("evaluate_expressions", {"expressions": "1"}, "sequence"),
        ("read_registers", {"names": [1]}, "string"),
        ("read_memory", {"regions": [{"address": "0"}]}, "length"),
        (
            "read_memory",
            {"regions": [{"address": "0", "length": 1, "access": "invalid"}]},
            "variant",
        ),
        ("read_debugger_window", {"window": "Register.view;Go"}, "variant"),
        (
            "read_debugger_window",
            {"window": "register", "command": "Go"},
            "unknown field",
        ),
        ("wait_for_condition", {"expression": "1"}, "expected_value"),
    ],
)
async def test_schema_errors_are_protocol_errors(
    native_mcp_server, tool_name, arguments, expected
):
    async with native_mcp_server as client:
        error = await tool_error(client, tool_name, arguments)
    assert expected in error, error


@pytest.mark.parametrize(
    ("skill_name", "script_name"),
    [
        ("../fixture", "read.cmm"),
        ("..\\fixture", "read.cmm"),
        ("C:\\fixture", "read.cmm"),
        ("fixture", "../read.cmm"),
        ("fixture", "..\\read.cmm"),
        ("fixture", "/read.cmm"),
        ("fixture", "C:\\read.cmm"),
        ("fixture", "read.cmm:stream"),
        ("fixture", 'read";END.cmm'),
        ("fixture", "read\x00.cmm"),
    ],
)
async def test_skill_path_traversal_rejected(mcp_client_factory, skill_name, script_name):
    async with mcp_client_factory("--execution-policy", "strict") as client:
        error = await tool_error(
            client,
            "trace32_run_script",
            {"skill_name": skill_name, "script_name": script_name},
        )
    assert "Invalid skill_name" in error or "Invalid script_name" in error, error
    assert "127.0.0.1:" not in error


@pytest.mark.parametrize(
    ("arguments", "expected"),
    [
        ({"bad key": "1"}, "Unknown argument"),
        ({"1key": "1"}, "Unknown argument"),
        ({"key": "1\nEND"}, "does not match the pattern"),
        ({"key": '" END'}, "does not match the pattern"),
        ({"key": "&macro"}, "does not match the pattern"),
    ],
)
async def test_skill_arguments_rejected_before_connecting(
    mcp_client_factory, arguments, expected
):
    async with mcp_client_factory("--execution-policy", "strict") as client:
        error = await tool_error(
            client,
            "trace32_run_script",
            {
                "skill_name": "fixture",
                "script_name": "read.cmm",
                "script_args": arguments,
            },
        )
    assert expected in error, error
    assert "127.0.0.1:" not in error


async def test_discover_skills_without_a_debugger(native_mcp_server, skills_base):
    async with native_mcp_server as client:
        result = await client.call_tool("list_practice_skills", {})
    response = result_object(result)
    assert response["truncated"] is False
    assert len(response["skills"]) == 1
    skill = response["skills"][0]
    assert skill["skill_name"] == "fixture"
    assert skill["scripts"] == ["read.cmm"]
    assert Path(skill["instructions_path"]).samefile(
        skills_base / "fixture" / "SKILL.md"
    )
    assert Path(skill["skill_directory"]).samefile(skills_base / "fixture")


async def test_status_with_absent_debugger_is_bounded(local_mcp_server, offline_port):
    async with local_mcp_server as client:
        started = time.monotonic()
        result = await asyncio.wait_for(
            client.call_tool(
                "trace32_get_status", {"timeout": 1}, raise_on_error=False
            ),
            timeout=10,
        )
        elapsed = time.monotonic() - started
    assert result.is_error
    assert elapsed < 3, elapsed
    text = result_text(result)
    # Some platforms refuse the connection immediately; others consume the
    # one-second request budget before a port-specific error can be returned.
    if "Request deadline exceeded" in text:
        assert "(not_started)" in text or "(unknown_completion)" in text
        if "(unknown_completion)" in text:
            assert "Do not automatically retry" in text
    else:
        assert f"port {offline_port}:" in text
    assert "127.0.0.1:20000" not in text


@pytest.mark.parametrize(
    "tool_name",
    ["trace32_run_script", "trace32_run_command", "trace32_manage_instances"],
)
@pytest.mark.parametrize("extra_args", [(), ("--native-tools",)])
async def test_read_only_rejects_direct_calls_to_hidden_tools(
    mcp_client_factory, tool_name, extra_args
):
    async with mcp_client_factory(*extra_args, "--read-only") as client:
        error = await tool_error(client, tool_name, {})
    assert any(
        phrase in error.lower()
        for phrase in ("not found", "unknown tool", "disabled", "not registered")
    ), error


@pytest.mark.parametrize("tool_name", sorted(NATIVE_TOOLS))
async def test_native_tools_cannot_be_called_without_opt_in(
    local_mcp_server, tool_name
):
    async with local_mcp_server as client:
        error = await tool_error(client, tool_name, {})
    assert any(
        phrase in error.lower()
        for phrase in ("not found", "unknown tool", "disabled", "not registered")
    ), error
    assert "127.0.0.1:" not in error


@pytest.mark.parametrize(
    "tool_name",
    [
        "execute_practice",
        "execute_practice_skill",
        "collect_practice_skill_response",
        "abort_practice_skill",
    ],
)
async def test_removed_legacy_tools_are_not_callable(local_mcp_server, tool_name):
    async with local_mcp_server as client:
        error = await tool_error(client, tool_name, {})
    assert any(
        phrase in error.lower()
        for phrase in ("not found", "unknown tool", "disabled", "not registered")
    ), error
