# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Execution target selection must reject ambiguity without probing PowerView."""

import asyncio
import selectors
import socket
import subprocess
import threading
from contextlib import ExitStack

import pytest
from conftest import DEFAULT_TOOLS
from fastmcp import Client
from fastmcp.client.transports import StdioTransport
from test_native_routing import connection_trap  # noqa: F401 (shared fixture)
from test_tools import tool_error


@pytest.fixture
def target_client_factory(server_executable, skills_base, tmp_path):
    def factory(ports, *extra_args):
        args = [
            "--execution-policy", "unrestricted",
            "--skills", str(skills_base),
            "--artifacts", str(tmp_path / "target-selection-artifacts"),
        ]
        for port in ports:
            args.extend(["--port", str(port)])
        return Client(
            StdioTransport(
                command=server_executable, args=[*args, *extra_args],
            )
        )

    return factory


@pytest.fixture
def registered_connection_traps():
    """Observe both configured endpoints, including the first candidate."""
    accepted = []
    stopped = threading.Event()
    with ExitStack() as cleanup:
        selector = cleanup.enter_context(selectors.DefaultSelector())
        listeners = [
            cleanup.enter_context(socket.socket(socket.AF_INET, socket.SOCK_STREAM))
            for _ in range(2)
        ]
        ports = []
        for listener in listeners:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            listener.setblocking(False)
            port = listener.getsockname()[1]
            ports.append(port)
            selector.register(listener, selectors.EVENT_READ, port)

        def drain():
            while not stopped.is_set():
                for key, _ in selector.select(timeout=0.05):
                    try:
                        connection, _ = key.fileobj.accept()
                    except BlockingIOError:
                        continue
                    with connection:
                        accepted.append(key.data)

        thread = threading.Thread(target=drain, daemon=True)
        thread.start()
        try:
            yield ports, accepted
        finally:
            stopped.set()
            thread.join(timeout=2)
            assert not thread.is_alive()


@pytest.fixture(
    params=["script", "collected-command", "fire-and-forget-command"]
)
def execution_request(request, skills_base):
    if request.param == "script":
        return (
            "trace32_run_script",
            {"script_path": str(skills_base / "fixture" / "scripts" / "read.cmm")},
        )
    return (
        "trace32_run_command",
        {
            "command": "PRINT 1.",
            "collect_output": request.param == "collected-command",
        },
    )


@pytest.mark.parametrize("selection", [{}, {"port": None}, {"core": 0}])
async def test_ambiguous_execution_rejects_before_connecting_to_any_instance(
    target_client_factory, registered_connection_traps, execution_request, selection
):
    ports, accepted = registered_connection_traps
    tool_name, arguments = execution_request
    async with target_client_factory(
        ports, "--require-explicit-target"
    ) as client:
        async with asyncio.timeout(10):
            error = await tool_error(
                client, tool_name, {**arguments, **selection}
            )
    assert "explicit port" in error, error
    assert "Connection with TRACE32" not in error, error
    assert accepted == []


@pytest.mark.parametrize("extra_args", [(), ("--require-explicit-target",)])
async def test_unregistered_execution_port_is_never_probed(
    target_client_factory, registered_connection_traps, connection_trap,
    execution_request, extra_args
):
    registered_ports, registered_accepted = registered_connection_traps
    port, accepted = connection_trap
    tool_name, arguments = execution_request
    async with target_client_factory(registered_ports, *extra_args) as client:
        async with asyncio.timeout(10):
            error = await tool_error(
                client, tool_name, {**arguments, "port": port}
            )
    assert f"No instance with port {port} is added" in error, error
    assert accepted == []
    assert registered_accepted == []


@pytest.mark.parametrize("selection", [{}, {"port": None}])
async def test_one_instance_does_not_require_explicit_execution_port(
    mcp_client_factory, offline_port, execution_request, selection
):
    tool_name, arguments = execution_request
    async with mcp_client_factory("--require-explicit-target") as client:
        async with asyncio.timeout(10):
            error = await tool_error(
                client, tool_name, {**arguments, **selection}
            )
    assert f"port {offline_port}:" in error, error
    assert "explicit port" not in error, error
    assert "No instance with port" not in error, error


async def test_explicit_execution_port_selects_only_that_registered_instance(
    mcp_client_factory, offline_port, second_offline_port, execution_request
):
    tool_name, arguments = execution_request
    async with mcp_client_factory(
        "--require-explicit-target", "--port", str(second_offline_port)
    ) as client:
        async with asyncio.timeout(10):
            error = await tool_error(
                client, tool_name, {**arguments, "port": second_offline_port}
            )
    assert f"port {second_offline_port}:" in error, error
    assert f"port {offline_port}:" not in error, error
    assert "explicit port" not in error, error


async def test_default_mode_retains_implicit_candidate_fallback(
    target_client_factory, registered_connection_traps, execution_request
):
    ports, accepted = registered_connection_traps
    tool_name, arguments = execution_request
    async with target_client_factory(ports) as client:
        async with asyncio.timeout(10):
            error = await tool_error(client, tool_name, arguments)
    # Test-owned listeners close immediately; Windows connect retries cannot
    # consume the request budget before the second candidate is exercised.
    for port in ports:
        assert f"port {port}:" in error, error
        assert port in accepted
    assert "explicit port" not in error, error


@pytest.mark.parametrize("extra_args", [(), ("--require-explicit-target",)])
async def test_native_target_ambiguity_is_unchanged(
    target_client_factory, registered_connection_traps, extra_args
):
    ports, accepted = registered_connection_traps
    async with target_client_factory(
        ports, "--native-tools", *extra_args
    ) as client:
        async with asyncio.timeout(10):
            error = await tool_error(client, "doctor", {})
    assert "explicit port" in error, error
    assert accepted == []


async def test_opt_in_preserves_tool_visibility_and_optional_port_schema(
    mcp_client_factory, second_offline_port
):
    schemas = []
    for extra_args in [(), ("--require-explicit-target",)]:
        async with mcp_client_factory(
            "--port", str(second_offline_port), *extra_args
        ) as client:
            tools = {tool.name: tool for tool in await client.list_tools()}
        assert set(tools) == DEFAULT_TOOLS
        execution_schemas = {
            name: tools[name].inputSchema
            for name in ("trace32_run_script", "trace32_run_command")
        }
        for schema in execution_schemas.values():
            assert "port" in schema["properties"]
            assert "port" not in schema.get("required", [])
        schemas.append(execution_schemas)
    assert schemas[0] == schemas[1]


@pytest.mark.parametrize("argument", ["--help", "-h"])
def test_explicit_target_mode_is_documented_in_cli_help(server_executable, argument):
    process = subprocess.run(
        [server_executable, argument],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=10, check=False,
    )
    assert process.returncode == 0, process.stderr
    assert "--require-explicit-target" in process.stdout


@pytest.mark.parametrize(
    "arguments",
    [
        ["--require-explicit-target=true"],
        ["--require-explicit-target", "false"],
    ],
)
def test_explicit_target_mode_does_not_accept_boolean_value_arguments(
    server_executable, offline_port, arguments
):
    process = subprocess.run(
        [
            server_executable, "--execution-policy", "unrestricted",
            "--port", str(offline_port), *arguments,
        ],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=10, check=False,
    )
    assert process.returncode != 0
    output = process.stdout + process.stderr
    assert "unknown argument" in output.lower(), output
    assert "panicked at" not in output
