# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

import re
import subprocess
from contextlib import ExitStack

import pytest
from conftest import (
    DEFAULT_TOOLS,
    NATIVE_TOOLS,
    READ_ONLY_TOOLS,
    SCRIPTS_TOOLS,
    STRICT_TOOLS,
    reserved_offline_port,
)
from fastmcp import Client
from fastmcp.client.transports import StdioTransport

TOOL_MODES = [
    ([], DEFAULT_TOOLS),
    (["--execution-policy", "scripts"], SCRIPTS_TOOLS),
    (["--execution-policy", "strict"], STRICT_TOOLS),
    (["--read-only"], READ_ONLY_TOOLS),
    (["--native-tools"], DEFAULT_TOOLS | NATIVE_TOOLS),
    (["--native-tools", "--execution-policy", "scripts"], SCRIPTS_TOOLS | NATIVE_TOOLS),
    (["--native-tools", "--execution-policy", "strict"], STRICT_TOOLS | NATIVE_TOOLS),
    (["--native-tools", "--read-only"], READ_ONLY_TOOLS | NATIVE_TOOLS),
    (
        ["--native-tools", "--read-only", "--execution-policy", "strict"],
        READ_ONLY_TOOLS | NATIVE_TOOLS,
    ),
    (
        ["--execution-policy", "strict", "--read-only", "--native-tools"],
        READ_ONLY_TOOLS | NATIVE_TOOLS,
    ),
]


async def test_server_connection(local_mcp_server: Client, remote_mcp_server: Client):
    async with local_mcp_server:
        local_info = local_mcp_server.initialize_result.serverInfo
    async with remote_mcp_server:
        remote_info = remote_mcp_server.initialize_result.serverInfo
    assert local_info == remote_info
    assert local_info.name == "t32mcp"
    assert local_info.version


async def test_list_tools(local_mcp_server: Client, remote_mcp_server: Client):
    async with local_mcp_server:
        local_tools = {tool.name: tool for tool in await local_mcp_server.list_tools()}
    async with remote_mcp_server:
        remote_tools = {tool.name: tool for tool in await remote_mcp_server.list_tools()}
    assert set(local_tools) == DEFAULT_TOOLS
    assert local_tools == remote_tools
    for tool in local_tools.values():
        assert tool.description, tool.name
        assert tool.inputSchema["type"] == "object", tool.name


@pytest.mark.parametrize(("arguments", "expected_tools"), TOOL_MODES)
async def test_tool_visibility(mcp_client_factory, arguments, expected_tools):
    async with mcp_client_factory(*arguments) as client:
        assert {tool.name for tool in await client.list_tools()} == expected_tools


@pytest.mark.parametrize(("arguments", "expected_tools"), TOOL_MODES)
async def test_http_tool_visibility(http_mcp_client_factory, arguments, expected_tools):
    with http_mcp_client_factory(*arguments) as client:
        async with client:
            assert {tool.name for tool in await client.list_tools()} == expected_tools


async def test_native_schemas_match_across_transports(native_mcp_server, http_mcp_client_factory):
    async with native_mcp_server as client:
        local_tools = {tool.name: tool for tool in await client.list_tools()}
    with http_mcp_client_factory("--native-tools") as client:
        async with client:
            remote_tools = {tool.name: tool for tool in await client.list_tools()}
    assert set(local_tools) == DEFAULT_TOOLS | NATIVE_TOOLS
    assert local_tools == remote_tools


@pytest.mark.parametrize("argument", ["--help", "-h", "--version"])
def test_cli_information_does_not_require_debugger(server_executable, argument):
    result = subprocess.run(
        [server_executable, argument], stdin=subprocess.DEVNULL,
        capture_output=True, text=True, timeout=10, check=False,
    )
    assert result.returncode == 0, result.stderr
    assert re.search(r"t32mcp v\d+\.\d+\.\d+", result.stdout)
    if argument != "--version":
        for option in (
            "--port", "--skills", "--http", "--execution-policy", "--token",
            "--native-tools", "--read-only", "--target-config", "--artifacts",
        ):
            assert option in result.stdout


@pytest.mark.parametrize(
    "arguments",
    [
        ["--unknown-option"], ["--port"], ["--port", "not-a-port"],
        ["--port", "65536"], ["--port", "0"], ["--http"], ["--http", "invalid"],
        ["--skills"], ["--skills", ""],
        ["--artifacts"], ["--artifacts", ""], ["--artifacts", "relative"],
        ["--http", "0"],
        ["--execution-policy"], ["--execution-policy", "invalid"], ["--target-config"],
    ],
)
def test_invalid_cli_arguments_fail_cleanly(server_executable, arguments):
    result = subprocess.run(
        [server_executable, "--execution-policy", "scripts", *arguments],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=10, check=False,
    )
    assert result.returncode != 0
    output = result.stdout + result.stderr
    assert output.strip()
    assert "panicked at" not in output


def test_execution_policy_is_required(server_executable, offline_port):
    result = subprocess.run(
        [server_executable, "--port", str(offline_port)], stdin=subprocess.DEVNULL,
        capture_output=True, text=True, timeout=10, check=False,
    )
    assert result.returncode != 0
    assert "--execution-policy" in result.stdout + result.stderr


@pytest.mark.parametrize(
    "arguments",
    [
        ["--t32chat"], ["--t32chat", "--read-only"], ["--native-tools", "--t32chat"],
        ["--execution-policy", "unrestricted", "--t32chat"],
    ],
)
def test_legacy_t32chat_requires_explicit_policy_migration(server_executable, arguments):
    result = subprocess.run(
        [server_executable, *arguments], stdin=subprocess.DEVNULL,
        capture_output=True, text=True, timeout=10, check=False,
    )
    assert result.returncode != 0
    output = result.stdout + result.stderr
    assert "--t32chat" in output
    assert "--execution-policy" in output
    assert "panicked at" not in output


@pytest.mark.parametrize(
    ("policy", "expected_tools"),
    [("scripts", SCRIPTS_TOOLS), ("unrestricted", DEFAULT_TOOLS)],
)
async def test_default_port_configuration_is_lazy(
    server_executable, policy, expected_tools
):
    # Never risk a startup regression connecting to an existing default session.
    with ExitStack() as cleanup:
        try:
            cleanup.enter_context(reserved_offline_port(20000))
        except OSError:
            pytest.skip("default port 20000 is already in use; do not probe it")
        client = Client(
            StdioTransport(
                command=server_executable, args=["--execution-policy", policy]
            )
        )
        async with client:
            assert {tool.name for tool in await client.list_tools()} == expected_tools
